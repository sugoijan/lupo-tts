// SPDX-License-Identifier: Apache-2.0
use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use lupo_audio::{AudioSink, DeviceSink};
use lupo_configuration::{
    SettingDomain, SettingRecord, SettingSource, SettingsService, SettingsSnapshot,
    SnapshotApplyOptions,
};
use lupo_persistence::{open_database_at_path, open_default_database};
use lupo_secrets::{load_secret_string, store_secret};
#[cfg(target_os = "macos")]
use lupo_tts_macos::{SystemSpeechProcessConfig, SystemSpeechProcessManager};
use lupo_tts_openai::OpenAi;
use lupo_tts_parler::{DevicePreference, ParlerLoadConfig, ParlerSynthesizer};
use messages_twitch::TwitchAuthConfig;
use serde_json::{self, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tracing::info;
use tracing_subscriber::EnvFilter;

mod twitch;

use crate::twitch::{
    DEFAULT_TWITCH_SCOPES, StoredAuthState, TwitchChatConfig, clear_auth_credentials,
    listen_to_twitch_chat, load_auth_metadata, perform_oauth_login, run_bttv_emotes,
    run_eventsub_chat, run_ffz_emotes, run_seventv_emotes,
};

// Twitch treats desktop apps as public OAuth clients, so embedding the client ID is expected for
// the device-code flow (no client secret involved).
const DEFAULT_TWITCH_CLIENT_ID: &str = "49dpqt2hwhs0gyitcm9azaaacpjpyz";

#[cfg(target_os = "macos")]
const DEFAULT_BACKEND: Backend = Backend::System;
#[cfg(not(target_os = "macos"))]
const DEFAULT_BACKEND: Backend = Backend::OpenAi;

#[derive(Parser, Debug)]
#[command(author, version, about = "LUPO CLI utilities", long_about = None)]
enum Cli {
    /// Synthesize text and play it through the default audio device.
    Speak(SpeakArgs),
    /// Inspect and manage LUPO settings.
    Settings(SettingsArgs),
    /// Manage encrypted secrets stored via the secure backing key.
    Secret(SecretArgs),
    /// Twitch integration utilities.
    Twitch(TwitchArgs),
}

#[derive(Args, Debug)]
struct SpeakArgs {
    /// Text that should be synthesized and played
    #[arg(value_name = "MESSAGE")]
    message: Option<String>,

    /// TTS backend to use
    #[arg(long, value_enum, default_value_t = DEFAULT_BACKEND)]
    backend: Backend,

    /// Hugging Face repository slug for the Parler backend
    #[arg(long, default_value = "parler-tts/parler-tts-mini-v1")]
    parler_repo: String,

    /// Optional revision (tag/commit) for the Parler repository
    #[arg(long)]
    parler_revision: Option<String>,

    /// Hugging Face access token (falls back to HF_TOKEN environment variable)
    #[arg(long, env = "HF_TOKEN")]
    parler_hf_token: Option<String>,

    /// Parler description prompt that controls speaker identity and style
    #[arg(long)]
    parler_description: Option<String>,

    /// Parler sampling temperature
    #[arg(long, default_value_t = 0.0)]
    parler_temperature: f64,

    /// Parler nucleus sampling probability cutoff
    #[arg(long)]
    parler_top_p: Option<f64>,

    /// Parler generation random seed
    #[arg(long, default_value_t = 0)]
    parler_seed: u64,

    /// Parler generation maximum decoding steps
    #[arg(long, default_value_t = 500)]
    parler_max_steps: usize,

    /// Force Parler inference to run on the CPU
    #[arg(long)]
    parler_cpu: bool,

    /// Attempt to run Parler inference on Apple's Metal backend
    #[cfg(target_os = "macos")]
    #[arg(long)]
    parler_metal: bool,
}

#[derive(Args, Debug)]
struct SettingsArgs {
    /// Override the data directory that stores the application database.
    #[arg(long)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: SettingsCommand,
}

#[derive(Args, Debug)]
struct SecretArgs {
    /// Override the data directory that stores the application database.
    #[arg(long)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: SecretCommand,
}

#[derive(Subcommand, Debug)]
enum SettingsCommand {
    /// List all known settings and their effective values.
    List {
        /// Restrict output to a specific settings domain.
        #[arg(long)]
        domain: Option<String>,
        /// Output format.
        #[arg(long, default_value = "table")]
        format: OutputFormat,
        /// Include entries that lack a schema definition.
        #[arg(long)]
        include_orphans: bool,
    },
    /// Show a single setting entry.
    Get {
        /// Settings key to inspect.
        key: String,
        /// Print only the raw value as JSON.
        #[arg(long)]
        value_only: bool,
        /// Output format for the metadata view.
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },
    /// Persist a value for the given key (value must be JSON).
    Set { key: String, value: String },
    /// Remove the persisted override for the given key.
    Reset { key: String },
    /// Remove every persisted override.
    ResetAll,
    /// Export all effective settings to a JSON snapshot.
    Export {
        path: PathBuf,
        #[arg(long)]
        pretty: bool,
    },
    /// Import settings from a previously exported snapshot.
    Import {
        path: PathBuf,
        #[arg(long)]
        allow_unknown: bool,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum Backend {
    #[cfg_attr(not(target_os = "macos"), clap(skip))]
    System,
    OpenAi,
    Parler,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    Table,
    Json,
}

#[derive(Subcommand, Debug)]
enum SecretCommand {
    /// Store a secret value.
    Set(SecretSetArgs),
    /// Retrieve a stored secret value.
    Get(SecretGetArgs),
}

#[derive(Args, Debug)]
struct SecretSetArgs {
    #[arg(value_enum)]
    name: SecretName,

    /// Secret value to store (use `--value` to pass inline).
    value: String,
}

#[derive(Args, Debug)]
struct SecretGetArgs {
    #[arg(value_enum)]
    name: SecretName,

    /// Print only the decrypted value without explanatory text.
    #[arg(long)]
    raw: bool,
}

#[derive(Args, Debug)]
struct TwitchArgs {
    /// Override the data directory that stores the application database.
    #[arg(long)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: TwitchCommand,
}

#[derive(Subcommand, Debug)]
enum TwitchCommand {
    /// Connect to Twitch chat and log messages for a channel.
    Listen(TwitchListenArgs),
    /// Manage Twitch OAuth authentication.
    Auth(TwitchAuthArgs),
    /// EventSub WebSocket utilities.
    Eventsub(TwitchEventArgs),
    /// Third-party emote utilities.
    Emotes(TwitchEmotesArgs),
}

#[derive(Args, Debug)]
struct TwitchListenArgs {
    /// Twitch channel to observe (without '#').
    channel: String,

    /// Twitch login for the account that will connect to IRC.
    #[arg(long, env = "TWITCH_LOGIN")]
    login: String,

    /// OAuth access token (prefix with 'oauth:' if not already present).
    #[arg(long, env = "TWITCH_OAUTH_TOKEN")]
    token: String,

    /// IRC endpoint host.
    #[arg(long, default_value = "irc.chat.twitch.tv")]
    host: String,

    /// IRC endpoint port.
    #[arg(long, default_value_t = 6667)]
    port: u16,

    /// Print raw IRC lines instead of formatted output.
    #[arg(long)]
    raw: bool,

    /// Skip capability negotiation for tags/commands/membership.
    #[arg(long)]
    no_capabilities: bool,
}

#[derive(Args, Debug)]
struct TwitchAuthArgs {
    #[command(subcommand)]
    command: TwitchAuthCommand,
}

#[derive(Subcommand, Debug)]
enum TwitchAuthCommand {
    /// Launch an interactive browser flow to authenticate with Twitch.
    Login(TwitchAuthLoginArgs),
    /// Display the currently linked Twitch account.
    Status,
    /// Remove stored Twitch credentials and metadata.
    Logout,
}

#[derive(Args, Debug)]
struct TwitchAuthLoginArgs {
    /// Twitch application client identifier.
    #[arg(
        long,
        env = "TWITCH_CLIENT_ID",
        default_value = DEFAULT_TWITCH_CLIENT_ID
    )]
    client_id: String,

    /// Disable automatic browser launch; prints the URL instead.
    #[arg(long)]
    no_browser: bool,

    /// Additional OAuth scopes to request (repeatable).
    #[arg(long, value_name = "SCOPE")]
    scope: Vec<String>,
}

#[derive(Args, Debug)]
struct TwitchEventArgs {
    #[command(subcommand)]
    command: TwitchEventCommand,
}

#[derive(Subcommand, Debug)]
enum TwitchEventCommand {
    /// Stream chat messages via EventSub WebSockets.
    Chat(TwitchEventChatArgs),
}

#[derive(Args, Debug)]
struct TwitchEventChatArgs {
    /// Twitch channel login to subscribe to (defaults to linked account).
    #[arg(long)]
    channel: Option<String>,

    /// Print only the concatenated text segments (omitting emotes).
    #[arg(long)]
    text_only: bool,
}

#[derive(Args, Debug)]
struct TwitchEmotesArgs {
    #[command(subcommand)]
    command: TwitchEmotesCommand,
}

#[derive(Subcommand, Debug)]
enum TwitchEmotesCommand {
    /// Inspect 7TV emotes (global or channel-scoped).
    #[command(name = "7tv")]
    SevenTv(TwitchSevenTvArgs),
    /// Inspect BetterTTV emotes (global or channel-scoped).
    #[command(name = "bttv")]
    BetterTtv(TwitchBetterTtvArgs),
    /// Inspect FrankerFaceZ emotes (global or channel-scoped).
    #[command(name = "ffz")]
    Ffz(TwitchFfzArgs),
}

#[derive(Args, Debug)]
struct TwitchSevenTvArgs {
    /// Twitch channel login to inspect (defaults to linked account).
    #[arg(long)]
    channel: Option<String>,

    /// Show the global 7TV set instead of channel emotes.
    #[arg(long)]
    global: bool,
}

#[derive(Args, Debug)]
struct TwitchBetterTtvArgs {
    /// Twitch channel login to inspect (defaults to linked account).
    #[arg(long)]
    channel: Option<String>,

    /// Show the global BetterTTV set instead of channel emotes.
    #[arg(long)]
    global: bool,
}

#[derive(Args, Debug)]
struct TwitchFfzArgs {
    /// Twitch channel login to inspect (defaults to linked account).
    #[arg(long)]
    channel: Option<String>,

    /// Show the global FFZ sets instead of channel emotes.
    #[arg(long)]
    global: bool,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "snake_case")]
enum SecretName {
    OpenAiApiKey,
    TwitchAccessToken,
    TwitchRefreshToken,
}

impl SecretName {
    fn setting_key(self) -> &'static str {
        match self {
            SecretName::OpenAiApiKey => "secret_openai_api_key",
            SecretName::TwitchAccessToken => "secret_twitch_access_token",
            SecretName::TwitchRefreshToken => "secret_twitch_refresh_token",
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // The dependency graph enables both rustls crypto backends; pick one before
    // any TLS config is built (see lupo-app for the same guard).
    let _ = rustls::crypto::ring::default_provider().install_default();
    init_tracing();

    match Cli::parse() {
        Cli::Speak(args) => run_speak(&args).await,
        Cli::Settings(args) => run_settings(args).await,
        Cli::Secret(args) => run_secret(args).await,
        Cli::Twitch(args) => run_twitch(args).await,
    }
}

async fn run_speak(args: &SpeakArgs) -> Result<()> {
    let message = args
        .message
        .clone()
        .unwrap_or_else(|| "Hello I'm ルポ-TTS.".to_string());

    let openai_api_key = if args.backend == Backend::OpenAi {
        Some(load_openai_api_key().await?)
    } else {
        None
    };
    #[cfg(target_os = "macos")]
    let system_manager = if args.backend == Backend::System {
        Some(
            SystemSpeechProcessManager::new(SystemSpeechProcessConfig::default())
                .context("failed to initialize system TTS helper manager")?,
        )
    } else {
        None
    };

    let audio_data = match args.backend {
        Backend::System => {
            #[cfg(target_os = "macos")]
            {
                let manager = system_manager
                    .as_ref()
                    .context("system TTS helper manager unavailable")?;
                synthesize_with_system(manager, &message).await?
            }
            #[cfg(not(target_os = "macos"))]
            {
                synthesize_with_system(&message).await?
            }
        }
        Backend::OpenAi => {
            let api_key = openai_api_key
                .as_deref()
                .context("OpenAI API key unavailable")?;
            synthesize_with_openai(&message, api_key).await?
        }
        Backend::Parler => synthesize_with_parler(args, &message).await?,
    };

    let audio = DeviceSink::start_default().context("failed to initialize audio output")?;

    audio
        .play_wav(audio_data.into())
        .await
        .context("failed to enqueue audio data for playback")?;

    audio
        .drain()
        .await
        .context("failed to wait for audio playback to finish")?;

    #[cfg(target_os = "macos")]
    if let Some(manager) = system_manager {
        let _ = manager.stop();
    }

    Ok(())
}

async fn open_settings_service(data_dir: Option<&Path>) -> Result<SettingsService> {
    let (db, base_path) = match data_dir {
        Some(path) => {
            let db = open_database_at_path(path)
                .await
                .with_context(|| format!("failed to open database under {}", path.display()))?;
            (db, path.to_path_buf())
        }
        None => {
            let (dirs, db) = open_default_database().await?;
            (db, dirs.data_local_dir().to_path_buf())
        }
    };

    info!("connected to settings database at {}", base_path.display());
    Ok(SettingsService::new(db))
}

async fn run_settings(args: SettingsArgs) -> Result<()> {
    let service = open_settings_service(args.data_dir.as_deref()).await?;

    match args.command {
        SettingsCommand::List {
            domain,
            format,
            include_orphans,
        } => {
            let listing = service.list()?;
            let filter = parse_domain(domain.as_deref())?;
            let entries: Vec<_> = listing
                .entries
                .into_iter()
                .filter(|entry| filter.is_none_or(|domain| entry.domain == domain))
                .collect();
            match format {
                OutputFormat::Table => print_table(&entries),
                OutputFormat::Json => print_json(&entries)?,
            }
            if include_orphans && !listing.orphans.is_empty() {
                eprintln!(
                    "\nOrphaned entries (keys present in the DB without a schema definition):"
                );
                for orphan in listing.orphans {
                    eprintln!(
                        "- {} (scope: {}, updated_at: {}) => {}",
                        orphan.key,
                        orphan.scope,
                        format_timestamp(&orphan.updated_at),
                        serde_json::to_string(&orphan.value)?
                    );
                }
            }
        }
        SettingsCommand::Get {
            key,
            value_only,
            format,
        } => {
            let record = service.get(&key)?;
            if value_only {
                println!("{}", serde_json::to_string_pretty(&record.value)?);
            } else {
                match format {
                    OutputFormat::Table => print_single(&record)?,
                    OutputFormat::Json => {
                        println!("{}", serde_json::to_string_pretty(&record)?);
                    }
                }
            }
        }
        SettingsCommand::Set { key, value } => {
            let parsed: Value = serde_json::from_str(&value).with_context(
                || "setting values must be expressed as JSON (e.g. \"true\", \"42\", or \"{...}\")",
            )?;
            service.set(&key, parsed)?;
            println!("Stored override for '{key}'.");
        }
        SettingsCommand::Reset { key } => {
            service.reset(&key)?;
            println!("Cleared override for '{key}'.");
        }
        SettingsCommand::ResetAll => {
            let removed = service.reset_all()?;
            println!("Cleared {removed} persisted overrides.");
        }
        SettingsCommand::Export { path, pretty } => {
            let snapshot = service.snapshot()?;
            write_snapshot(&path, &snapshot, pretty)?;
            println!("Exported settings snapshot to {}", path.display());
        }
        SettingsCommand::Import {
            path,
            allow_unknown,
            dry_run,
        } => {
            let snapshot = read_snapshot(&path)?;
            let report = service.apply_snapshot(
                &snapshot,
                SnapshotApplyOptions {
                    allow_unknown,
                    dry_run,
                },
            )?;
            if dry_run {
                println!("Dry run complete; no changes persisted.");
            }
            if report.applied.is_empty() {
                println!("No setting changes were required.");
            } else {
                println!("Applied {} setting changes:", report.applied.len());
                for change in report.applied {
                    println!(
                        "- {} (from {} to {})",
                        change.key,
                        serde_json::to_string(&change.previous_value)?,
                        serde_json::to_string(&change.new_value)?
                    );
                }
            }
            if !report.skipped_unknown.is_empty() {
                println!(
                    "Skipped {} unknown keys: {}",
                    report.skipped_unknown.len(),
                    report.skipped_unknown.join(", ")
                );
            }
        }
    }

    Ok(())
}

async fn run_twitch(args: TwitchArgs) -> Result<()> {
    let TwitchArgs { data_dir, command } = args;
    match command {
        TwitchCommand::Listen(listen) => {
            let channel = listen.channel.trim_start_matches('#').trim();
            if channel.is_empty() {
                bail!("channel name must not be empty");
            }
            let login = listen.login.trim();
            if login.is_empty() {
                bail!("login must not be empty");
            }
            let token = listen.token.trim();
            if token.is_empty() {
                bail!("token must not be empty");
            }

            let config = TwitchChatConfig {
                login: login.to_owned(),
                token: token.to_owned(),
                channel: channel.to_owned(),
                host: listen.host,
                port: listen.port,
                request_capabilities: !listen.no_capabilities,
                raw: listen.raw,
            };

            listen_to_twitch_chat(config).await
        }
        TwitchCommand::Auth(auth_args) => run_twitch_auth(auth_args, data_dir).await,
        TwitchCommand::Eventsub(event_args) => run_twitch_eventsub(event_args, data_dir).await,
        TwitchCommand::Emotes(emote_args) => run_twitch_emotes(emote_args, data_dir).await,
    }
}

async fn run_twitch_auth(args: TwitchAuthArgs, data_dir: Option<PathBuf>) -> Result<()> {
    let service = open_settings_service(data_dir.as_deref()).await?;

    match args.command {
        TwitchAuthCommand::Login(login) => {
            let mut scopes: Vec<String> = if login.scope.is_empty() {
                DEFAULT_TWITCH_SCOPES
                    .iter()
                    .map(|scope| scope.to_string())
                    .collect()
            } else {
                login.scope.clone()
            };
            scopes.sort();
            scopes.dedup();

            let config =
                TwitchAuthConfig::new(login.client_id.clone()).with_default_scopes(scopes.clone());

            let outcome = perform_oauth_login(&service, config, &scopes, login.no_browser).await?;

            let display_name = outcome
                .login_state
                .display_name
                .as_deref()
                .unwrap_or(&outcome.login_state.login);

            println!(
                "Linked Twitch account '{display_name}' (user id {}).",
                outcome.login_state.user_id
            );
            if outcome.login_state.scopes.is_empty() {
                println!("Granted scopes: default");
            } else {
                println!("Granted scopes: {}", outcome.login_state.scopes.join(", "));
            }
        }
        TwitchAuthCommand::Status => match load_auth_metadata(&service)? {
            Some(state) => print_twitch_status(state),
            None => println!(
                "No Twitch account is currently linked. Run `lupo-cli twitch auth login` to connect."
            ),
        },
        TwitchAuthCommand::Logout => {
            clear_auth_credentials(&service).await?;
            println!("Cleared stored Twitch credentials.");
        }
    }

    Ok(())
}

async fn run_twitch_eventsub(args: TwitchEventArgs, data_dir: Option<PathBuf>) -> Result<()> {
    let service = open_settings_service(data_dir.as_deref()).await?;

    match args.command {
        TwitchEventCommand::Chat(chat_args) => {
            run_eventsub_chat(&service, chat_args.channel, chat_args.text_only).await?
        }
    }

    Ok(())
}

async fn run_twitch_emotes(args: TwitchEmotesArgs, data_dir: Option<PathBuf>) -> Result<()> {
    let service = open_settings_service(data_dir.as_deref()).await?;

    match args.command {
        TwitchEmotesCommand::SevenTv(opts) => {
            run_seventv_emotes(&service, opts.channel, opts.global).await?
        }
        TwitchEmotesCommand::BetterTtv(opts) => {
            run_bttv_emotes(&service, opts.channel, opts.global).await?
        }
        TwitchEmotesCommand::Ffz(opts) => {
            run_ffz_emotes(&service, opts.channel, opts.global).await?
        }
    }

    Ok(())
}

fn print_twitch_status(state: StoredAuthState) {
    let display_name = state
        .login_state
        .display_name
        .as_deref()
        .unwrap_or(&state.login_state.login);
    println!(
        "Twitch account: {} (user id {})",
        display_name, state.login_state.user_id
    );

    if state.login_state.scopes.is_empty() {
        println!("Scopes: (default)");
    } else {
        println!("Scopes: {}", state.login_state.scopes.join(", "));
    }

    println!(
        "Linked at: {}",
        format_timestamp(&state.login_state.obtained_at)
    );

    match state.token_info.expires_at {
        Some(ts) => println!("Access token expires at: {}", format_timestamp(&ts)),
        None => match state.token_info.expires_in {
            Some(secs) => println!("Access token expires in: {}s", secs),
            None => println!("Access token expiry: not provided by Twitch"),
        },
    }

    println!(
        "Refresh token: {}",
        if state.token_info.has_refresh_token {
            "stored"
        } else {
            "not stored"
        }
    );
}

async fn run_secret(args: SecretArgs) -> Result<()> {
    let service = open_settings_service(args.data_dir.as_deref()).await?;

    match args.command {
        SecretCommand::Set(args) => {
            store_secret(&service, args.name.setting_key(), args.value.as_bytes())
                .map_err(|err| anyhow!(err.to_string()))?;
            println!(
                "Stored secret '{}' in the secure store.",
                args.name.setting_key()
            );
        }
        SecretCommand::Get(args) => match load_secret_string(&service, args.name.setting_key()) {
            Ok(Some(secret)) => {
                if args.raw {
                    println!("{secret}");
                } else {
                    println!("Secret '{}': {secret}", args.name.setting_key());
                }
            }
            Ok(None) => {
                bail!("Secret '{}' is not set", args.name.setting_key());
            }
            Err(err) => {
                return Err(anyhow!(err.to_string()));
            }
        },
    }

    Ok(())
}

#[cfg(target_os = "macos")]
async fn synthesize_with_system(
    manager: &SystemSpeechProcessManager,
    message: &str,
) -> Result<Vec<u8>> {
    let manager = manager.clone();
    let text = message.to_string();
    tokio::task::spawn_blocking(move || manager.synthesize_to_wav(&text))
        .await
        .context("system synthesis worker task failed to join")?
        .map_err(|err| anyhow!(err.to_string()))
}

#[cfg(not(target_os = "macos"))]
async fn synthesize_with_system(_message: &str) -> Result<Vec<u8>> {
    bail!("system backend is only available on macOS")
}

async fn synthesize_with_openai(message: &str, api_key: &str) -> Result<Vec<u8>> {
    let tts = OpenAi::new(api_key.to_string());
    let audio_data = tts
        .text_to_speech(message)
        .await
        .context("failed to synthesize speech with OpenAI")?;

    Ok(audio_data.to_vec())
}

async fn synthesize_with_parler(args: &SpeakArgs, message: &str) -> Result<Vec<u8>> {
    let mut load_config = ParlerLoadConfig {
        repo: args.parler_repo.clone(),
        revision: args.parler_revision.clone(),
        hf_token: args.parler_hf_token.clone(),
        ..ParlerLoadConfig::default()
    };
    if let Some(description) = &args.parler_description {
        load_config.default_description = description.clone();
    }
    load_config.device = parler_device_preference(args);
    load_config.generation.temperature = args.parler_temperature;
    load_config.generation.top_p = args.parler_top_p;
    load_config.generation.seed = args.parler_seed;
    load_config.generation.max_steps = args.parler_max_steps;

    let text = message.to_owned();

    tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut synthesizer = ParlerSynthesizer::new(load_config)?;
        synthesizer.synthesize_to_wav(&text, None, None)
    })
    .await
    .context("parler synthesis task failed to join")?
}

fn parler_device_preference(args: &SpeakArgs) -> DevicePreference {
    if args.parler_cpu {
        return DevicePreference::Cpu;
    }
    #[cfg(target_os = "macos")]
    {
        if args.parler_metal {
            return DevicePreference::Metal;
        }
    }
    DevicePreference::Auto
}

async fn load_openai_api_key() -> Result<String> {
    let (_dirs, db) = open_default_database()
        .await
        .context("failed to open settings database")?;
    let service = SettingsService::new(db);
    match load_secret_string(&service, "secret_openai_api_key") {
        Ok(Some(secret)) => Ok(secret),
        Ok(None) => Err(anyhow!(
            "OpenAI API key is not configured. Run `lupo-cli secret set open_ai_api_key <value>` to store it."
        )),
        Err(err) => Err(anyhow!(
            "failed to load OpenAI API key from local secret store: {err}"
        )),
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .try_init();
}

fn parse_domain(domain: Option<&str>) -> Result<Option<SettingDomain>> {
    match domain {
        None => Ok(None),
        Some(value) => SettingDomain::from_str(value)
            .map(Some)
            .map_err(|_| anyhow!("unknown settings domain '{value}'")),
    }
}

fn print_table(entries: &[SettingRecord]) {
    if entries.is_empty() {
        println!("No settings matched the requested filters.");
        return;
    }

    let mut key_width = "Key".len();
    let mut domain_width = "Domain".len();
    let mut risk_width = "Risk".len();
    let mut source_width = "Source".len();

    let rows: Vec<_> = entries
        .iter()
        .map(|entry| {
            let source = render_source(&entry.source);
            let value = serde_json::to_string(&entry.value).unwrap_or_else(|_| "<invalid>".into());
            key_width = key_width.max(entry.key.len());
            domain_width = domain_width.max(entry.domain.as_str().len());
            risk_width = risk_width.max(entry.risk.as_str().len());
            source_width = source_width.max(source.len());
            (entry, source, value)
        })
        .collect();

    println!(
        "{:<key$}  {:<domain$}  {:<risk$}  {:<source$}  Value",
        "Key",
        "Domain",
        "Risk",
        "Source",
        key = key_width,
        domain = domain_width,
        risk = risk_width,
        source = source_width
    );
    println!(
        "{}  {}  {}  {}  {}",
        "-".repeat(key_width),
        "-".repeat(domain_width),
        "-".repeat(risk_width),
        "-".repeat(source_width),
        "-".repeat(5)
    );

    for (entry, source, value) in rows {
        println!(
            "{:<key$}  {:<domain$}  {:<risk$}  {:<source$}  {}",
            entry.key,
            entry.domain.as_str(),
            entry.risk.as_str(),
            source,
            value,
            key = key_width,
            domain = domain_width,
            risk = risk_width,
            source = source_width
        );
    }
}

fn print_json(entries: &[SettingRecord]) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(entries)?);
    Ok(())
}

fn print_single(record: &SettingRecord) -> Result<()> {
    println!("Key: {}", record.key);
    println!("Title: {}", record.title);
    println!("Domain: {}", record.domain.as_str());
    println!("Risk: {}", record.risk.as_str());
    println!("Stored value source: {}", render_source(&record.source));
    println!("Value:\n{}", serde_json::to_string_pretty(&record.value)?);
    Ok(())
}

fn render_source(source: &SettingSource) -> String {
    match source {
        SettingSource::Default => "default".to_string(),
        SettingSource::Persisted { scope, updated_at } => {
            let formatted = format_timestamp(updated_at);
            format!("persisted ({scope}, {formatted})")
        }
    }
}

fn format_timestamp(ts: &OffsetDateTime) -> String {
    ts.format(&Rfc3339).unwrap_or_else(|_| format!("{ts:?}"))
}

fn write_snapshot(path: &Path, snapshot: &SettingsSnapshot, pretty: bool) -> Result<()> {
    let mut file =
        fs::File::create(path).with_context(|| format!("unable to create {}", path.display()))?;
    if pretty {
        let contents = serde_json::to_string_pretty(snapshot)?;
        file.write_all(contents.as_bytes())?;
    } else {
        let contents = serde_json::to_vec(snapshot)?;
        file.write_all(&contents)?;
    }
    Ok(())
}

fn read_snapshot(path: &Path) -> Result<SettingsSnapshot> {
    let data = fs::read(path).with_context(|| format!("unable to read {}", path.display()))?;
    serde_json::from_slice(&data)
        .with_context(|| format!("invalid snapshot JSON in {}", path.display()))
}

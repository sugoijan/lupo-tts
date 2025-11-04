use crate::DEFAULT_TWITCH_CLIENT_ID;
use anyhow::{Context, Result, anyhow, bail};
use emotes_7tv::{SevenTvCatalog, SevenTvClient, SevenTvEmoteSet};
use emotes_bttv::{BttvCatalog, BttvChannelEmoteSets, BttvClient, BttvEmoteSet};
use emotes_ffz::{FfzCatalog, FfzClient, FfzEmoteSet, FfzGlobalEmoteSets};
use lupo_configuration::{SettingSource, SettingsService};
use lupo_secrets::{SecureStoreError, delete_secret, load_secret_string, store_secret};
use messages::{EmojiResolver, EmoteModifier, EmoteModifierFlags, EmoteResolver, ResolverRegistry};
use messages_twitch::{
    ChatMessage as EventSubChatMessage, ChatSubscriptionConfig as EventSubChatSubscriptionConfig,
    EventSubAuthError, EventSubStreamEvent, TwitchAuthClient, TwitchAuthConfig, TwitchAuthOutcome,
    TwitchLoginState, TwitchTokenBundle, TwitchTokenStorage, TwitchUserIdentity,
    connect_chat_stream, fetch_user_identity, is_twitch_unauthorized,
    refresh_access_token as twitch_refresh_access_token,
    validate_access_token as twitch_validate_access_token,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::borrow::ToOwned;
use std::cmp::Reverse;
use std::convert::TryInto;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use time::{Duration as TimeDuration, OffsetDateTime};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::signal;
use tracing::{debug, info, warn};

const SECRET_TWITCH_ACCESS_KEY: &str = "secret_twitch_access_token";
const SECRET_TWITCH_REFRESH_KEY: &str = "secret_twitch_refresh_token";
const TWITCH_AUTH_STATE_KEY: &str = "twitch_auth_state";
pub const DEFAULT_TWITCH_SCOPES: &[&str] = &[
    "chat:read",
    "bits:read",
    "channel:read:redemptions",
    "moderator:read:chatters",
    "user:read:chat",
];

pub async fn perform_oauth_login(
    service: &SettingsService,
    config: TwitchAuthConfig,
    scopes: &[String],
    suppress_browser: bool,
) -> Result<TwitchAuthOutcome> {
    let client = TwitchAuthClient::new(config)?;
    let authorization = client.start_device_code_flow(scopes).await?;

    println!(
        "To connect LUPO to Twitch, visit {} and enter the code {}.",
        authorization.verification_uri, authorization.user_code
    );

    if !suppress_browser
        && let Some(url) = authorization
            .verification_uri_complete
            .as_deref()
            .or(Some(authorization.verification_uri.as_str()))
    {
        if let Err(err) = webbrowser::open(url) {
            warn!(?err, "failed to open Twitch verification URL");
        } else {
            println!("Opened your browser to {url}");
        }
    }

    println!("Waiting for Twitch authorization to complete...");
    let outcome = client.complete_device_code_flow(&authorization).await?;
    let storage = SettingsBackedTokenStorage::new(service);
    storage
        .store_tokens(&outcome.login_state, &outcome.tokens)
        .await?;
    Ok(outcome)
}

pub fn load_auth_metadata(service: &SettingsService) -> Result<Option<StoredAuthState>> {
    read_metadata(service)
}

pub async fn clear_auth_credentials(service: &SettingsService) -> Result<()> {
    let storage = SettingsBackedTokenStorage::new(service);
    storage.clear_tokens().await?;
    Ok(())
}

pub async fn load_stored_credentials(
    service: &SettingsService,
) -> Result<Option<(TwitchLoginState, TwitchTokenBundle)>> {
    let storage = SettingsBackedTokenStorage::new(service);
    storage.load_tokens().await
}

struct SettingsBackedTokenStorage {
    service: SettingsService,
}

impl SettingsBackedTokenStorage {
    fn new(service: &SettingsService) -> Self {
        Self {
            service: service.clone(),
        }
    }

    fn secure_error(err: SecureStoreError) -> anyhow::Error {
        anyhow!(err.to_string())
    }
}

impl TwitchTokenStorage for SettingsBackedTokenStorage {
    fn store_tokens<'s>(
        &'s self,
        state: &'s TwitchLoginState,
        tokens: &'s TwitchTokenBundle,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 's>> {
        let service = self.service.clone();
        Box::pin(async move {
            if tokens.access_token.is_empty() {
                bail!("Twitch returned an empty access token");
            }

            let metadata = StoredAuthState::from_parts(state, tokens);
            let value = serde_json::to_value(&metadata)
                .map_err(|err| anyhow!("failed to serialize Twitch auth metadata: {err}"))?;

            store_secret(
                &service,
                SECRET_TWITCH_ACCESS_KEY,
                tokens.access_token.as_bytes(),
            )
            .map_err(Self::secure_error)?;

            if let Some(refresh) = tokens.refresh_token.as_ref() {
                store_secret(&service, SECRET_TWITCH_REFRESH_KEY, refresh.as_bytes())
                    .map_err(Self::secure_error)?;
            } else {
                delete_secret(&service, SECRET_TWITCH_REFRESH_KEY).map_err(Self::secure_error)?;
            }

            service
                .set(TWITCH_AUTH_STATE_KEY, value)
                .map_err(|err| anyhow!(err.to_string()))?;

            Ok(())
        })
    }

    fn load_tokens(
        &self,
    ) -> Pin<
        Box<dyn Future<Output = Result<Option<(TwitchLoginState, TwitchTokenBundle)>>> + Send + '_>,
    > {
        let service = self.service.clone();
        Box::pin(async move {
            let Some(metadata) = read_metadata(&service)? else {
                return Ok(None);
            };

            let access_token = load_secret_string(&service, SECRET_TWITCH_ACCESS_KEY)
                .map_err(Self::secure_error)?;
            let Some(access_token) = access_token else {
                return Ok(None);
            };

            let refresh_token = load_secret_string(&service, SECRET_TWITCH_REFRESH_KEY)
                .map_err(Self::secure_error)?;

            let tokens = TwitchTokenBundle {
                access_token,
                refresh_token,
                expires_in: metadata.token_info.expires_in,
                scope: metadata.login_state.scopes.clone(),
                token_type: metadata.token_info.token_type.clone(),
            };

            Ok(Some((metadata.login_state, tokens)))
        })
    }

    fn clear_tokens(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        let service = self.service.clone();
        Box::pin(async move {
            delete_secret(&service, SECRET_TWITCH_ACCESS_KEY).map_err(Self::secure_error)?;
            delete_secret(&service, SECRET_TWITCH_REFRESH_KEY).map_err(Self::secure_error)?;
            service
                .reset(TWITCH_AUTH_STATE_KEY)
                .map_err(|err| anyhow!(err.to_string()))?;
            Ok(())
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTokenInfo {
    pub token_type: String,
    pub expires_in: Option<u64>,
    pub expires_at: Option<OffsetDateTime>,
    pub has_refresh_token: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAuthState {
    pub login_state: TwitchLoginState,
    pub token_info: StoredTokenInfo,
}

impl StoredAuthState {
    fn from_parts(state: &TwitchLoginState, tokens: &TwitchTokenBundle) -> Self {
        Self {
            login_state: state.clone(),
            token_info: StoredTokenInfo::from_parts(state, tokens),
        }
    }
}

impl StoredTokenInfo {
    fn from_parts(state: &TwitchLoginState, tokens: &TwitchTokenBundle) -> Self {
        let expires_at = tokens
            .expires_in
            .and_then(|secs| secs.try_into().ok())
            .and_then(|secs: i64| state.obtained_at.checked_add(TimeDuration::seconds(secs)));

        Self {
            token_type: tokens.token_type.clone(),
            expires_in: tokens.expires_in,
            expires_at,
            has_refresh_token: tokens
                .refresh_token
                .as_ref()
                .map(|value| !value.is_empty())
                .unwrap_or(false),
        }
    }
}

fn read_metadata(service: &SettingsService) -> Result<Option<StoredAuthState>> {
    let record = service
        .get(TWITCH_AUTH_STATE_KEY)
        .map_err(|err| anyhow!(err.to_string()))?;
    let source = record.source;
    let value = record.value;

    if matches!(source, SettingSource::Default) {
        return Ok(None);
    }

    if value.as_object().map(|obj| obj.is_empty()).unwrap_or(false) {
        return Ok(None);
    }

    let metadata: StoredAuthState = serde_json::from_value(value)
        .map_err(|err| anyhow!("invalid Twitch auth metadata stored in settings: {err}"))?;

    Ok(Some(metadata))
}

async fn ensure_valid_access_token(
    service: &SettingsService,
    http: &Client,
    state: &TwitchLoginState,
    tokens: &TwitchTokenBundle,
) -> Result<TwitchTokenBundle> {
    if let Some(validation) = twitch_validate_access_token(http, &tokens.access_token)
        .await
        .context("failed to validate stored Twitch token")?
    {
        debug!(
            expires_in = validation.expires_in,
            "Twitch access token validated successfully"
        );
        return Ok(tokens.clone());
    }

    debug!("Stored Twitch access token rejected by Twitch validation; attempting refresh");
    refresh_access_token_and_store(service, http, state, tokens).await
}

async fn refresh_tokens_after_unauthorized(
    service: &SettingsService,
    http: &Client,
    state: &TwitchLoginState,
    tokens: &TwitchTokenBundle,
    context: &str,
) -> Result<TwitchTokenBundle> {
    match twitch_validate_access_token(http, &tokens.access_token)
        .await
        .with_context(|| format!("failed to validate Twitch token after {context} returned 401"))?
    {
        Some(validation) => {
            debug!(
                expires_in = validation.expires_in,
                reason = context,
                "Twitch token was still reported valid despite HTTP 401"
            );
        }
        None => {
            debug!(
                reason = context,
                "Twitch token confirmed invalid by validation endpoint after HTTP 401"
            );
        }
    }

    refresh_access_token_and_store(service, http, state, tokens).await
}

async fn refresh_access_token_and_store(
    service: &SettingsService,
    http: &Client,
    state: &TwitchLoginState,
    tokens: &TwitchTokenBundle,
) -> Result<TwitchTokenBundle> {
    let refresh_token = tokens.refresh_token.as_deref().ok_or_else(|| {
        anyhow!("Stored Twitch login cannot be refreshed; run `lupo-cli twitch auth login`.")
    })?;

    let Some(new_tokens) =
        twitch_refresh_access_token(http, DEFAULT_TWITCH_CLIENT_ID, refresh_token)
            .await
            .context("failed to refresh Twitch access token")?
    else {
        bail!("Twitch rejected the stored refresh token; run `lupo-cli twitch auth login`.");
    };

    let storage = SettingsBackedTokenStorage::new(service);
    storage
        .store_tokens(state, &new_tokens)
        .await
        .context("failed to persist refreshed Twitch tokens")?;

    Ok(new_tokens)
}

pub async fn listen_to_twitch_chat(config: TwitchChatConfig) -> Result<()> {
    let TwitchChatConfig {
        login,
        token,
        channel,
        host,
        port,
        request_capabilities,
        raw,
    } = config;

    let address = format!("{}:{}", host, port);
    let stream = TcpStream::connect(&address)
        .await
        .with_context(|| format!("failed to connect to Twitch IRC at {address}"))?;

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let channel = channel.to_lowercase();
    let nick = login.to_lowercase();

    send_line(
        &mut write_half,
        &format!("PASS {}", normalize_token(&token)),
    )
    .await?;
    send_line(&mut write_half, &format!("NICK {}", nick)).await?;

    if request_capabilities {
        send_line(
            &mut write_half,
            "CAP REQ :twitch.tv/tags twitch.tv/commands twitch.tv/membership",
        )
        .await?;
    }

    send_line(&mut write_half, &format!("JOIN #{}", channel)).await?;

    println!("Listening to Twitch chat on #{channel} as {login}. Press Ctrl+C to stop.");

    pump_messages(&mut reader, &mut write_half, raw).await?;

    let part_command = format!("PART #{}", channel);
    if let Err(err) = send_line(&mut write_half, &part_command).await {
        debug!(?err, "failed to send PART command");
    }
    if let Err(err) = send_line(&mut write_half, "QUIT").await {
        debug!(?err, "failed to send QUIT command");
    }
    if let Err(err) = write_half.shutdown().await {
        debug!(?err, "failed to shutdown Twitch IRC writer");
    }

    info!("disconnected from Twitch IRC");
    Ok(())
}

pub struct TwitchChatConfig {
    pub login: String,
    pub token: String,
    pub channel: String,
    pub host: String,
    pub port: u16,
    pub request_capabilities: bool,
    pub raw: bool,
}

async fn pump_messages(
    reader: &mut BufReader<OwnedReadHalf>,
    writer: &mut OwnedWriteHalf,
    raw: bool,
) -> Result<()> {
    let mut buffer = String::new();

    loop {
        buffer.clear();
        tokio::select! {
            read_result = reader.read_line(&mut buffer) => {
                let bytes = read_result.context("failed to read from Twitch IRC")?;
                if bytes == 0 {
                    info!("Twitch IRC closed the connection");
                    break;
                }

                let line = buffer.trim_end_matches(&['\r', '\n'][..]);
                if line.is_empty() {
                    continue;
                }

                if handle_ping(line, writer).await? {
                    continue;
                }

                if raw {
                    println!("{line}");
                    continue;
                }

                if let Some(message) = parse_privmsg(line) {
                    print_privmsg(&message);
                } else if let Some(notice) = parse_notice(line) {
                    println!("[notice] {notice}");
                } else {
                    debug!(line, "twitch server message");
                }
            }
            _ = signal::ctrl_c() => {
                info!("received interrupt; closing Twitch IRC connection");
                break;
            }
        }
    }

    Ok(())
}

async fn send_line(writer: &mut OwnedWriteHalf, line: &str) -> Result<()> {
    writer
        .write_all(line.as_bytes())
        .await
        .context("failed to send IRC command")?;
    writer
        .write_all(b"\r\n")
        .await
        .context("failed to write IRC newline")?;
    writer
        .flush()
        .await
        .context("failed to flush Twitch IRC writer")?;
    Ok(())
}

async fn handle_ping(line: &str, writer: &mut OwnedWriteHalf) -> Result<bool> {
    if let Some(payload) = line.strip_prefix("PING") {
        let payload = payload.trim_start();
        let response = if payload.is_empty() {
            "PONG".to_string()
        } else {
            format!("PONG {payload}")
        };
        send_line(writer, &response).await?;
        return Ok(true);
    }
    Ok(false)
}

struct ParsedPrivmsg {
    channel: String,
    login: String,
    display_name: Option<String>,
    badges: Vec<String>,
    text: String,
}

fn parse_privmsg(line: &str) -> Option<ParsedPrivmsg> {
    let (tags_section, mut remainder) = split_tags(line)?;

    if !remainder.starts_with(':') {
        return None;
    }

    let prefix_end = remainder.find(' ')?;
    let prefix = &remainder[1..prefix_end];
    remainder = remainder[prefix_end + 1..].trim_start();

    if !remainder.starts_with("PRIVMSG ") {
        return None;
    }

    remainder = &remainder["PRIVMSG ".len()..];
    let message_sep = remainder.find(" :")?;
    let channel = remainder[..message_sep]
        .trim()
        .trim_start_matches('#')
        .to_string();
    let text = remainder[message_sep + 2..].to_string();

    let login = prefix.split('!').next().unwrap_or(prefix).to_string();

    let mut display_name = None;
    let mut badges = Vec::new();

    if let Some(tags) = tags_section {
        for tag in tags.split(';') {
            let mut parts = tag.splitn(2, '=');
            let key = parts.next().unwrap_or_default();
            let value = parts.next().unwrap_or_default();

            if key.is_empty() {
                continue;
            }

            match key {
                "display-name" if !value.is_empty() => {
                    display_name = Some(unescape_irc_tag(value));
                }
                "badges" if !value.is_empty() => {
                    badges = value
                        .split(',')
                        .filter(|entry| !entry.is_empty())
                        .map(ToOwned::to_owned)
                        .collect();
                }
                _ => {}
            }
        }
    }

    Some(ParsedPrivmsg {
        channel,
        login,
        display_name,
        badges,
        text,
    })
}

fn split_tags(line: &str) -> Option<(Option<&str>, &str)> {
    if let Some(stripped) = line.strip_prefix('@') {
        let space = stripped.find(' ')?;
        let tags = &stripped[..space];
        let remainder = stripped[space + 1..].trim_start();
        Some((Some(tags), remainder))
    } else {
        Some((None, line.trim_start()))
    }
}

fn parse_notice(line: &str) -> Option<String> {
    let (_, mut remainder) = split_tags(line)?;

    if remainder.starts_with(':') {
        let prefix_end = remainder.find(' ')?;
        remainder = remainder[prefix_end + 1..].trim_start();
    }

    if !remainder.starts_with("NOTICE ") {
        return None;
    }

    remainder = &remainder["NOTICE ".len()..];
    let text = remainder.split_once(" :")?.1;
    Some(unescape_irc_tag(text))
}

fn print_privmsg(message: &ParsedPrivmsg) {
    let name = message.display_name.as_deref().unwrap_or(&message.login);

    if message.badges.is_empty() {
        println!("[#{}] {}: {}", message.channel, name, message.text);
    } else {
        let badges = message.badges.join(", ");
        println!(
            "[#{}] {} ({}): {}",
            message.channel, name, badges, message.text
        );
    }
}

fn normalize_token(token: &str) -> String {
    if token.len() >= 6 && token[..6].eq_ignore_ascii_case("oauth:") {
        format!("oauth:{}", &token[6..])
    } else {
        format!("oauth:{token}")
    }
}

fn unescape_irc_tag(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                match next {
                    's' => result.push(' '),
                    ':' => result.push(';'),
                    'r' => result.push('\r'),
                    'n' => result.push('\n'),
                    '\\' => result.push('\\'),
                    other => {
                        result.push('\\');
                        result.push(other);
                    }
                }
            } else {
                result.push('\\');
            }
        } else {
            result.push(ch);
        }
    }
    result
}

fn print_eventsub_chat_message(message: &EventSubChatMessage, text_only: bool) {
    let speaker = if message.chatter_display_name.is_empty() {
        &message.chatter_login
    } else {
        &message.chatter_display_name
    };
    let channel = if message.broadcaster_display_name.is_empty() {
        &message.broadcaster_login
    } else {
        &message.broadcaster_display_name
    };

    let rendered_text = if text_only {
        message.body.text_only()
    } else {
        message.body.raw_text.clone()
    };

    if message.badges.is_empty() || text_only {
        println!("[#{}] {}: {}", channel, speaker, rendered_text);
    } else {
        let badges = message
            .badges
            .iter()
            .map(|badge| format!("{}:{}", badge.set_id, badge.id))
            .collect::<Vec<_>>()
            .join(", ");
        println!("[#{}] {} ({}): {}", channel, speaker, badges, rendered_text);
    }
}

fn print_seventv_emote_set(set: &SevenTvEmoteSet) {
    println!(
        "7TV emote set \"{}\" ({} emotes)",
        set.name,
        set.emotes.len()
    );
    if set.emotes.is_empty() {
        println!("  (no emotes found)");
        return;
    }
    for emote in &set.emotes {
        let preview = emote
            .images
            .first()
            .map(|img| img.url.as_str())
            .unwrap_or("<no preview>");
        println!("  - {} ({}) {}", emote.code, emote.id.value, preview);
    }
}

fn print_bttv_emote_set(set: &BttvEmoteSet, indent: &str) {
    println!(
        "{indent}BetterTTV emote set \"{}\" ({} emotes)",
        set.name,
        set.emotes.len()
    );
    if set.emotes.is_empty() {
        println!("{indent}  (no emotes found)");
        return;
    }
    for emote in &set.emotes {
        let preview = emote
            .images
            .first()
            .map(|img| img.url.as_str())
            .unwrap_or("<no preview>");
        println!(
            "{indent}  - {} ({}) {}",
            emote.code, emote.id.value, preview
        );
    }
}

fn print_bttv_channel_sets(sets: &BttvChannelEmoteSets) {
    if sets.sets.is_empty() {
        println!("BetterTTV emote sets: (none)");
        return;
    }

    if sets.sets.len() == 1 {
        print_bttv_emote_set(&sets.sets[0], "");
        return;
    }

    println!("BetterTTV emote sets ({} total):", sets.sets.len());
    for set in &sets.sets {
        print_bttv_emote_set(set, "  ");
    }
}

fn print_ffz_emote_set(set: &FfzEmoteSet, indent: &str) {
    let name = set.title.as_deref().unwrap_or("<untitled>");
    println!(
        "{indent}FFZ emote set \"{name}\" ({} emotes)",
        set.emotes.len()
    );
    if set.emotes.is_empty() {
        println!("{indent}  (no emotes found)");
        return;
    }
    for emote in &set.emotes {
        let preview = emote
            .images
            .first()
            .map(|img| img.url.as_str())
            .unwrap_or("<no preview>");
        if let Some(modifier) = emote.modifier() {
            let descriptor = describe_ffz_modifier(modifier);
            println!(
                "{indent}  - {} ({}) {} [modifier: {}]",
                emote.code, emote.id.value, preview, descriptor
            );
        } else {
            println!(
                "{indent}  - {} ({}) {}",
                emote.code, emote.id.value, preview
            );
        }
    }
}

fn describe_ffz_modifier(modifier: &EmoteModifier) -> String {
    let mut labels = Vec::new();
    if modifier.is_hidden {
        labels.push("hidden".to_string());
    }
    const FLAG_LABELS: &[(EmoteModifierFlags, &str)] = &[
        (EmoteModifierFlags::FLIP_X, "flip_x"),
        (EmoteModifierFlags::FLIP_Y, "flip_y"),
        (EmoteModifierFlags::GROW_X, "grow_x"),
        (EmoteModifierFlags::RAINBOW, "rainbow"),
        (EmoteModifierFlags::HYPER_RED, "hyper_red"),
        (EmoteModifierFlags::HYPER_SHAKE, "hyper_shake"),
        (EmoteModifierFlags::CURSED, "cursed"),
        (EmoteModifierFlags::JAM, "jam"),
        (EmoteModifierFlags::BOUNCE, "bounce"),
    ];
    for (flag, label) in FLAG_LABELS {
        if modifier.flags.contains(*flag) {
            labels.push((*label).to_string());
        }
    }
    let unknown_bits = modifier.raw_flags & !EmoteModifierFlags::all().bits();
    if unknown_bits != 0 {
        labels.push(format!("unknown:0x{unknown_bits:x}"));
    }
    if !modifier.mask_images.is_empty() {
        labels.push(format!("masks:{}", modifier.mask_images.len()));
    }
    if labels.is_empty() {
        labels.push("none".to_string());
    }
    labels.join("|")
}

fn print_ffz_global_sets(sets: &FfzGlobalEmoteSets) {
    let default_sets = sets.default_sets();
    println!("FFZ default global sets ({} total):", default_sets.len());
    if default_sets.is_empty() {
        println!("  (no default global sets)");
    } else {
        for set in &default_sets {
            print_ffz_emote_set(set.as_ref(), "  ");
        }
    }

    let mut summary = sets.user_scoped_summary();
    if summary.is_empty() {
        println!("FFZ user-scoped global sets: (none)");
    } else {
        summary.sort_by_key(|(_, count)| Reverse(*count));
        let total_assignments: usize = summary.iter().map(|(_, count)| *count).sum();
        println!(
            "FFZ user-scoped global sets ({} unique, {} total assignments):",
            summary.len(),
            total_assignments
        );
        const PREVIEW_LIMIT: usize = 10;
        for (set, count) in summary.iter().take(PREVIEW_LIMIT) {
            let name = set.title.as_deref().unwrap_or("<untitled>");
            println!(
                "  - \"{}\" ({} emotes) assigned to {} users",
                name,
                set.emotes.len(),
                count
            );
        }
        if summary.len() > PREVIEW_LIMIT {
            println!(
                "  ... plus {} additional sets. Use `--channel <login>` to inspect a specific broadcaster.",
                summary.len() - PREVIEW_LIMIT
            );
        }
    }
}

#[derive(Default)]
struct ProviderResolvers {
    global: Option<Arc<dyn EmoteResolver>>,
    channel: Option<Arc<dyn EmoteResolver>>,
}

impl ProviderResolvers {
    fn is_empty(&self) -> bool {
        self.global.is_none() && self.channel.is_none()
    }
}

async fn build_seventv_resolvers(channel_id: &str, channel_login: &str) -> ProviderResolvers {
    let client = match SevenTvClient::new() {
        Ok(client) => client,
        Err(err) => {
            warn!(
                ?err,
                "7TV client initialization failed; skipping third-party emotes"
            );
            return ProviderResolvers::default();
        }
    };

    let mut catalog = SevenTvCatalog::new();

    match client.fetch_global_emotes().await {
        Ok(set) => catalog.insert_global_set(&set),
        Err(err) => warn!(?err, "failed to fetch 7TV global emotes"),
    }

    match client.fetch_channel_emotes_by_twitch_id(channel_id).await {
        Ok(set) => catalog.insert_channel_set(channel_id.to_string(), &set),
        Err(err) => warn!(
            ?err,
            channel = channel_login,
            "failed to fetch 7TV channel emotes"
        ),
    }

    if catalog.is_empty() {
        ProviderResolvers::default()
    } else {
        ProviderResolvers {
            global: catalog.global_resolver(),
            channel: catalog.channel_resolver(channel_id),
        }
    }
}

async fn build_bttv_resolvers(channel_id: &str, channel_login: &str) -> ProviderResolvers {
    let client = match BttvClient::new() {
        Ok(client) => client,
        Err(err) => {
            warn!(
                ?err,
                "BetterTTV client initialization failed; skipping BetterTTV emotes"
            );
            return ProviderResolvers::default();
        }
    };

    let mut catalog = BttvCatalog::new();

    match client.fetch_global_emotes().await {
        Ok(set) => {
            catalog.insert_global_emotes(set.emotes.iter().cloned());
        }
        Err(err) => warn!(?err, "failed to fetch BetterTTV global emotes"),
    }

    match client.fetch_channel_emotes_by_twitch_id(channel_id).await {
        Ok(channel_sets) => {
            let emotes = channel_sets.all_emotes();
            if !emotes.is_empty() {
                catalog.insert_channel_emotes(channel_id.to_string(), emotes);
            }
        }
        Err(err) => warn!(
            ?err,
            channel = channel_login,
            "failed to fetch BetterTTV channel emotes"
        ),
    }

    if catalog.is_empty() {
        ProviderResolvers::default()
    } else {
        ProviderResolvers {
            global: catalog.global_resolver(),
            channel: catalog.channel_resolver(channel_id),
        }
    }
}

async fn build_ffz_resolvers(channel_id: &str, channel_login: &str) -> ProviderResolvers {
    let client = match FfzClient::new() {
        Ok(client) => client,
        Err(err) => {
            warn!(
                ?err,
                "FFZ client initialization failed; skipping FrankerFaceZ emotes"
            );
            return ProviderResolvers::default();
        }
    };

    let mut catalog = FfzCatalog::new();

    match client.fetch_global_emotes().await {
        Ok(sets) => {
            let default_sets = sets.default_sets();
            if !default_sets.is_empty() {
                catalog.insert_global_sets(default_sets.clone());
            }
            let user_sets = sets.sets_for_user(channel_id);
            if !user_sets.is_empty() {
                catalog.insert_channel_sets(channel_id.to_string(), user_sets.clone());
            }
        }
        Err(err) => warn!(?err, "failed to fetch FFZ global emotes"),
    }

    match client.fetch_room_emotes_by_twitch_id(channel_id).await {
        Ok(room_sets) => {
            catalog.insert_channel_sets(channel_id.to_string(), room_sets.sets.iter());
        }
        Err(err) => warn!(
            ?err,
            channel = channel_login,
            "failed to fetch FFZ channel emotes"
        ),
    }

    if catalog.is_empty() {
        ProviderResolvers::default()
    } else {
        ProviderResolvers {
            global: catalog.global_resolver(),
            channel: catalog.channel_resolver(channel_id),
        }
    }
}

pub async fn run_eventsub_chat(
    service: &SettingsService,
    channel_login: Option<String>,
    text_only: bool,
) -> Result<()> {
    let Some((login_state, stored_tokens)) = load_stored_credentials(service).await? else {
        bail!("No stored Twitch credentials. Run `lupo-cli twitch auth login` first.");
    };

    let broadcaster_login = channel_login
        .map(|s| s.to_lowercase())
        .unwrap_or_else(|| login_state.login.to_lowercase());

    let http = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("failed to build HTTP client")?;

    let mut tokens =
        ensure_valid_access_token(service, &http, &login_state, &stored_tokens).await?;

    let broadcaster = if broadcaster_login.eq_ignore_ascii_case(&login_state.login) {
        TwitchUserIdentity {
            id: login_state.user_id.clone(),
            login: login_state.login.clone(),
            display_name: login_state
                .display_name
                .clone()
                .unwrap_or_else(|| login_state.login.clone()),
        }
    } else {
        match fetch_user_identity(
            &http,
            DEFAULT_TWITCH_CLIENT_ID,
            &tokens.access_token,
            &broadcaster_login,
        )
        .await
        {
            Ok(identity) => identity,
            Err(err) if is_twitch_unauthorized(&err) => {
                tokens = refresh_tokens_after_unauthorized(
                    service,
                    &http,
                    &login_state,
                    &tokens,
                    "fetching channel identity",
                )
                .await?;
                fetch_user_identity(
                    &http,
                    DEFAULT_TWITCH_CLIENT_ID,
                    &tokens.access_token,
                    &broadcaster_login,
                )
                .await?
            }
            Err(err) => return Err(err),
        }
    };

    println!(
        "Connecting to EventSub WebSocket for channel {}...",
        broadcaster.display_name
    );

    let mut resolver_registry = ResolverRegistry::new();
    resolver_registry.add_global(EmojiResolver::shared());

    let seven_tv = build_seventv_resolvers(&broadcaster.id, &broadcaster.login).await;
    if !seven_tv.is_empty() {
        info!(
            "7TV emotes enabled for channel {}",
            broadcaster.display_name
        );
    }
    if let Some(global) = seven_tv.global {
        resolver_registry.add_global(global);
    }
    if let Some(channel) = seven_tv.channel {
        resolver_registry.add_channel(broadcaster.id.clone(), channel);
    }

    let bttv = build_bttv_resolvers(&broadcaster.id, &broadcaster.login).await;
    if !bttv.is_empty() {
        info!(
            "BetterTTV emotes enabled for channel {}",
            broadcaster.display_name
        );
    }
    if let Some(global) = bttv.global {
        resolver_registry.add_global(global);
    }
    if let Some(channel) = bttv.channel {
        resolver_registry.add_channel(broadcaster.id.clone(), channel);
    }

    let ffz = build_ffz_resolvers(&broadcaster.id, &broadcaster.login).await;
    if !ffz.is_empty() {
        info!(
            "FFZ emotes enabled for channel {}",
            broadcaster.display_name
        );
    }
    if let Some(global) = ffz.global {
        resolver_registry.add_global(global);
    }
    if let Some(channel) = ffz.channel {
        resolver_registry.add_channel(broadcaster.id.clone(), channel);
    }

    let emote_resolver = Arc::new(resolver_registry);

    let mut subscription_config = EventSubChatSubscriptionConfig {
        client_id: DEFAULT_TWITCH_CLIENT_ID.to_string(),
        access_token: tokens.access_token.clone(),
        broadcaster_id: broadcaster.id.clone(),
        user_id: login_state.user_id.clone(),
        emote_resolver: Some(Arc::clone(&emote_resolver)),
    };

    let mut stream = match connect_chat_stream(&http, subscription_config.clone()).await {
        Ok(stream) => stream,
        Err(err) if err.downcast_ref::<EventSubAuthError>().is_some() => {
            tokens = refresh_tokens_after_unauthorized(
                service,
                &http,
                &login_state,
                &tokens,
                "creating EventSub chat subscription",
            )
            .await?;
            subscription_config.access_token = tokens.access_token.clone();
            connect_chat_stream(&http, subscription_config.clone()).await?
        }
        Err(err) => return Err(err),
    };

    println!(
        "Subscribed to channel {} chat via EventSub (session {}). Press Ctrl+C to stop.",
        broadcaster.display_name,
        stream.session_id()
    );

    loop {
        tokio::select! {
            event = stream.next_event() => {
                match event? {
                    Some(EventSubStreamEvent::ChatMessage(message)) => {
                        print_eventsub_chat_message(&message, text_only);
                    }
                    Some(EventSubStreamEvent::MessageDeleted(deleted)) => {
                        debug!(
                            message_id = %deleted.message_id,
                            broadcaster_id = %deleted.broadcaster_id,
                            "received EventSub message delete event"
                        );
                    }
                    Some(EventSubStreamEvent::Keepalive) => {
                        debug!("received EventSub keepalive");
                    }
                    Some(EventSubStreamEvent::SessionReconnect { reconnect_url }) => {
                        warn!(%reconnect_url, "Twitch requested EventSub reconnect; stopping listener");
                        break;
                    }
                    Some(EventSubStreamEvent::SessionDisconnect { status, reason }) => {
                        warn!(%status, ?reason, "EventSub session disconnected by Twitch");
                        break;
                    }
                    Some(EventSubStreamEvent::Revocation { status, reason }) => {
                        bail!(
                            "EventSub subscription was revoked by Twitch (status {:?}, reason {:?})",
                            status,
                            reason
                        );
                    }
                    None => {
                        warn!("EventSub stream ended by Twitch");
                        break;
                    }
                }
            }
            _ = signal::ctrl_c() => {
                info!("Stopping EventSub listener.");
                if let Err(err) = stream.close().await {
                    debug!(?err, "failed to close EventSub WebSocket cleanly");
                }
                break;
            }
        }
    }

    Ok(())
}

pub async fn run_bttv_emotes(
    service: &SettingsService,
    channel_login: Option<String>,
    global: bool,
) -> Result<()> {
    let client = BttvClient::new().context("failed to initialize BetterTTV client")?;

    if global {
        let set = client
            .fetch_global_emotes()
            .await
            .context("failed to fetch BetterTTV global emotes")?;
        print_bttv_emote_set(&set, "");
        return Ok(());
    }

    let Some((login_state, stored_tokens)) = load_stored_credentials(service).await? else {
        bail!("No stored Twitch credentials. Run `lupo-cli twitch auth login` first.");
    };

    let http = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("failed to build HTTP client")?;

    let mut tokens =
        ensure_valid_access_token(service, &http, &login_state, &stored_tokens).await?;

    let target_login = channel_login.unwrap_or_else(|| login_state.login.clone());

    let twitch_id = if target_login.eq_ignore_ascii_case(&login_state.login) {
        login_state.user_id.clone()
    } else {
        let identity = match fetch_user_identity(
            &http,
            DEFAULT_TWITCH_CLIENT_ID,
            &tokens.access_token,
            &target_login,
        )
        .await
        {
            Ok(identity) => identity,
            Err(err) if is_twitch_unauthorized(&err) => {
                tokens = refresh_tokens_after_unauthorized(
                    service,
                    &http,
                    &login_state,
                    &tokens,
                    "fetching channel identity for BetterTTV",
                )
                .await?;
                fetch_user_identity(
                    &http,
                    DEFAULT_TWITCH_CLIENT_ID,
                    &tokens.access_token,
                    &target_login,
                )
                .await?
            }
            Err(err) => return Err(err),
        };
        identity.id
    };

    let sets = client
        .fetch_channel_emotes_by_twitch_id(&twitch_id)
        .await
        .with_context(|| format!("failed to fetch BetterTTV emotes for channel {target_login}"))?;

    print_bttv_channel_sets(&sets);

    Ok(())
}

pub async fn run_seventv_emotes(
    service: &SettingsService,
    channel_login: Option<String>,
    global: bool,
) -> Result<()> {
    let client = SevenTvClient::new().context("failed to initialize 7tv client")?;

    if global {
        let set = client
            .fetch_global_emotes()
            .await
            .context("failed to fetch 7tv global emotes")?;
        print_seventv_emote_set(&set);
        return Ok(());
    }

    let Some((login_state, stored_tokens)) = load_stored_credentials(service).await? else {
        bail!("No stored Twitch credentials. Run `lupo-cli twitch auth login` first.");
    };

    let http = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("failed to build HTTP client")?;

    let mut tokens =
        ensure_valid_access_token(service, &http, &login_state, &stored_tokens).await?;

    let target_login = channel_login.unwrap_or_else(|| login_state.login.clone());

    let twitch_id = if target_login.eq_ignore_ascii_case(&login_state.login) {
        login_state.user_id.clone()
    } else {
        let identity = match fetch_user_identity(
            &http,
            DEFAULT_TWITCH_CLIENT_ID,
            &tokens.access_token,
            &target_login,
        )
        .await
        {
            Ok(identity) => identity,
            Err(err) if is_twitch_unauthorized(&err) => {
                tokens = refresh_tokens_after_unauthorized(
                    service,
                    &http,
                    &login_state,
                    &tokens,
                    "fetching channel identity for 7tv",
                )
                .await?;
                fetch_user_identity(
                    &http,
                    DEFAULT_TWITCH_CLIENT_ID,
                    &tokens.access_token,
                    &target_login,
                )
                .await?
            }
            Err(err) => return Err(err),
        };
        identity.id
    };

    let set = client
        .fetch_channel_emotes_by_twitch_id(&twitch_id)
        .await
        .with_context(|| format!("failed to fetch 7tv emotes for channel {target_login}"))?;

    print_seventv_emote_set(&set);

    Ok(())
}

pub async fn run_ffz_emotes(
    service: &SettingsService,
    channel_login: Option<String>,
    global: bool,
) -> Result<()> {
    let client = FfzClient::new().context("failed to initialize FFZ client")?;

    if global {
        let sets = client
            .fetch_global_emotes()
            .await
            .context("failed to fetch FFZ global emotes")?;
        print_ffz_global_sets(&sets);
        return Ok(());
    }

    let Some((login_state, stored_tokens)) = load_stored_credentials(service).await? else {
        bail!("No stored Twitch credentials. Run `lupo-cli twitch auth login` first.");
    };

    let http = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("failed to build HTTP client")?;

    let mut tokens =
        ensure_valid_access_token(service, &http, &login_state, &stored_tokens).await?;

    let target_login = channel_login.unwrap_or_else(|| login_state.login.clone());

    let identity = if target_login.eq_ignore_ascii_case(&login_state.login) {
        TwitchUserIdentity {
            id: login_state.user_id.clone(),
            login: login_state.login.clone(),
            display_name: login_state
                .display_name
                .clone()
                .unwrap_or_else(|| login_state.login.clone()),
        }
    } else {
        match fetch_user_identity(
            &http,
            DEFAULT_TWITCH_CLIENT_ID,
            &tokens.access_token,
            &target_login,
        )
        .await
        {
            Ok(identity) => identity,
            Err(err) if is_twitch_unauthorized(&err) => {
                tokens = refresh_tokens_after_unauthorized(
                    service,
                    &http,
                    &login_state,
                    &tokens,
                    "fetching channel identity for ffz",
                )
                .await?;
                fetch_user_identity(
                    &http,
                    DEFAULT_TWITCH_CLIENT_ID,
                    &tokens.access_token,
                    &target_login,
                )
                .await?
            }
            Err(err) => return Err(err),
        }
    };

    let global_sets = match client.fetch_global_emotes().await {
        Ok(sets) => Some(sets),
        Err(err) => {
            warn!(?err, "failed to fetch FFZ global emotes");
            None
        }
    };

    let room_sets = client
        .fetch_room_emotes_by_twitch_id(&identity.id)
        .await
        .with_context(|| format!("failed to fetch FFZ emotes for channel {target_login}"))?;

    if let Some(global_sets) = &global_sets {
        let default_sets = global_sets.default_sets();
        println!(
            "FFZ default global sets available to all channels: {}",
            default_sets.len()
        );
        for set in &default_sets {
            print_ffz_emote_set(set.as_ref(), "  ");
        }
        let user_sets = global_sets.sets_for_user(&identity.id);
        if !user_sets.is_empty() {
            println!("FFZ user-scoped global sets for {}:", identity.display_name);
            for set in &user_sets {
                print_ffz_emote_set(set.as_ref(), "  ");
            }
        }
    }

    println!("FFZ channel emote sets for {}:", identity.display_name);
    if room_sets.sets.is_empty() {
        println!("  (no emotes found)");
    } else {
        for set in &room_sets.sets {
            print_ffz_emote_set(set, "  ");
        }
    }

    Ok(())
}

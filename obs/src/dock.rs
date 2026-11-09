// SPDX-License-Identifier: Apache-2.0

//! The local web server behind the LUPO dock: serves the Dioxus (wasm) UI from
//! the plugin bundle's data directory and speaks `lupo_runtime::ui_protocol`
//! over `/ws`.
//!
//! - Binds `127.0.0.1` only, on the first free port from [`DEFAULT_PORT`].
//! - The page and the WebSocket require a token (`?token=…`), generated once
//!   into the plugin's config directory: any local page could otherwise open a
//!   WebSocket to `localhost` and control the queue (or set secrets). Static
//!   assets (wasm/js) are served without the token; they contain no state.
//! - The dock is registered natively through `csrc/dock_shim.cpp`; on OBS
//!   builds without obs-browser the URL can be used as a Custom Browser Dock.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use lupo_configuration::SettingsService;
use lupo_runtime::pipeline::{MessagePipeline, OPENAI_SECRET_KEY, PipelineEvent};
use lupo_runtime::ui_protocol::{
    ClientCommand, ProviderView, SecretKind, ServerEvent, message_view, playback_view,
    provider_view,
};
use rand::Rng;
use tokio::net::TcpListener;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::chat::ChatSupervisor;

pub const DEFAULT_PORT: u16 = 27055;
const PORT_ATTEMPTS: u16 = 10;
const TOKEN_FILE: &str = "dock-token";
const PROBE_TEXT: &str = "This is a LUPO test.";

struct DockState {
    handle: Handle,
    pipeline: Arc<MessagePipeline>,
    chat: Arc<ChatSupervisor>,
    settings: SettingsService,
    token: String,
    assets_dir: PathBuf,
}

pub struct DockServer {
    pub url: String,
    /// `Some(reason)` when the dock could not be added to OBS's Docks menu
    /// natively and the user has to add a Custom Browser Dock by hand instead.
    pub manual_reason: Option<String>,
}

pub const NATIVE_DOCK_ID: &CStr = c"lupo_tts";
const NATIVE_DOCK_TITLE: &CStr = c"LUPO TTS";

unsafe extern "C" {
    fn lupo_native_dock_qcef_version() -> c_int;
    fn lupo_native_dock_create(
        id: *const c_char,
        title: *const c_char,
        url: *const c_char,
    ) -> c_int;
    fn lupo_native_dock_remove(id: *const c_char);
}

/// Add the page as a native dock through obs-browser's panel API (see
/// `csrc/dock_shim.cpp`). UI thread only.
fn add_native_dock(url: &str) -> Result<(), String> {
    let url = CString::new(url).map_err(|_| "url contains NUL".to_string())?;
    // SAFETY: C strings outlive the call; the shim is called on the UI thread
    // from obs_module_post_load, after obs-browser has loaded.
    let code = unsafe {
        lupo_native_dock_create(
            NATIVE_DOCK_ID.as_ptr(),
            NATIVE_DOCK_TITLE.as_ptr(),
            url.as_ptr(),
        )
    };
    match code {
        0 => Ok(()),
        1 => {
            warn!(
                "dock added, but its parent could not be made a native window; tabbed docks may misbehave"
            );
            Ok(())
        }
        -1 => Err("obs-browser is not loaded (this OBS build has no browser panels)".into()),
        -2 => Err("obs-browser has no module library".into()),
        -3 => Err(format!(
            "obs-browser panel API too old (version {})",
            // SAFETY: plain FFI query.
            unsafe { lupo_native_dock_qcef_version() }
        )),
        -9 => Err("obs-browser does not export a panel API version".into()),
        -4 => Err("obs-browser does not export obs_browser_create_qcef".into()),
        -5 => Err("obs-browser failed to create a panel context".into()),
        -6 => Err("CEF failed to initialise".into()),
        -7 => Err("obs-browser failed to create the dock widget".into()),
        -8 => Err("OBS refused the dock (id already in use?)".into()),
        other => Err(format!("unknown shim error {other}")),
    }
}

static NATIVE_DOCK_LIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Remove the native dock. Must run on the UI thread while CEF is still up —
/// `OBS_FRONTEND_EVENT_EXIT` fires after OBS cleared its own browser docks and
/// before it deletes the CEF context; destroying the widget any later crashes
/// inside obs-browser's widget destructor. Idempotent.
pub fn remove_native_dock() {
    if !NATIVE_DOCK_LIVE.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    // SAFETY: obs_frontend_remove_dock null-checks the frontend callbacks and
    // deletes the dock (and our widget) synchronously.
    unsafe { lupo_native_dock_remove(NATIVE_DOCK_ID.as_ptr()) };
    info!("native dock removed");
}

#[derive(Debug, thiserror::Error)]
pub enum DockError {
    #[error("no free port in {DEFAULT_PORT}..{}", DEFAULT_PORT + PORT_ATTEMPTS)]
    NoPort,
    #[error("failed to read or write the dock token: {0}")]
    Token(std::io::Error),
}

/// Bind and start serving on the engine runtime.
pub fn start(
    handle: &Handle,
    pipeline: Arc<MessagePipeline>,
    chat: Arc<ChatSupervisor>,
    settings: SettingsService,
    config_dir: &Path,
    assets_dir: PathBuf,
) -> Result<DockServer, DockError> {
    let token = load_or_create_token(config_dir).map_err(DockError::Token)?;
    let listener = handle
        .block_on(async {
            for port in DEFAULT_PORT..DEFAULT_PORT + PORT_ATTEMPTS {
                if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
                    return Some(listener);
                }
            }
            None
        })
        .ok_or(DockError::NoPort)?;
    let port = listener
        .local_addr()
        .map(|addr| addr.port())
        .unwrap_or(DEFAULT_PORT);
    let state = Arc::new(DockState {
        handle: handle.clone(),
        pipeline,
        chat,
        settings,
        token: token.clone(),
        assets_dir,
    });
    let app = Router::new()
        .route("/", get(page))
        .route("/assets/{*file}", get(asset))
        .route("/ws", get(websocket))
        .with_state(state);
    handle.spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            warn!(%err, "dock server stopped");
        }
    });
    let url = format!("http://127.0.0.1:{port}/?token={token}");
    info!(port, "dock server listening");
    let manual_reason = add_native_dock(&url).err();
    match &manual_reason {
        None => {
            NATIVE_DOCK_LIVE.store(true, std::sync::atomic::Ordering::SeqCst);
            info!("native dock added to the Docks menu")
        }
        Some(err) => {
            warn!(%err, "native dock unavailable; add a Custom Browser Dock manually")
        }
    }
    Ok(DockServer { url, manual_reason })
}

fn load_or_create_token(config_dir: &Path) -> std::io::Result<String> {
    let path = config_dir.join(TOKEN_FILE);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if trimmed.len() >= 16 {
            return Ok(trimmed.to_string());
        }
    }
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::create_dir_all(config_dir)?;
    std::fs::write(&path, &token)?;
    Ok(token)
}

fn authorized(state: &DockState, query: &HashMap<String, String>) -> bool {
    query.get("token").map(String::as_str) == Some(state.token.as_str())
}

async fn page(
    State(state): State<Arc<DockState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !authorized(&state, &query) {
        return (StatusCode::FORBIDDEN, "missing or wrong token").into_response();
    }
    match tokio::fs::read_to_string(state.assets_dir.join("index.html")).await {
        Ok(index) => Html(index).into_response(),
        Err(err) => {
            warn!(%err, dir = %state.assets_dir.display(), "dock UI assets missing");
            Html(
                "<!doctype html><title>LUPO TTS</title><body style=\"color-scheme:dark;font-family:system-ui\">\
                 <h3>LUPO dock UI assets are missing.</h3>\
                 <p>Rebuild and reinstall the plugin: <code>cargo xtask bundle-obs --install</code></p>",
            )
            .into_response()
        }
    }
}

async fn asset(
    State(state): State<Arc<DockState>>,
    axum::extract::Path(file): axum::extract::Path<String>,
) -> Response {
    // Nested paths are allowed (wasm-bindgen emits a snippets/ tree), but every
    // component must be a plain file name.
    if file
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == ".." || part.contains('\\'))
    {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let mime = match file.rsplit_once('.').map(|(_, ext)| ext) {
        Some("wasm") => "application/wasm",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        _ => "application/octet-stream",
    };
    match tokio::fs::read(state.assets_dir.join(&file)).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, mime)], bytes).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

async fn websocket(
    State(state): State<Arc<DockState>>,
    Query(query): Query<HashMap<String, String>>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !authorized(&state, &query) {
        return (StatusCode::FORBIDDEN, "missing or wrong token").into_response();
    }
    upgrade.on_upgrade(move |socket| session(socket, state))
}

fn secret_store_key(secret: SecretKind) -> &'static str {
    match secret {
        SecretKind::OpenAiApiKey => OPENAI_SECRET_KEY,
    }
}

/// Terminal (played/skipped/errored) messages included in a snapshot. Pending
/// ones are always included; without a cap, a multi-hour stream would replay
/// its entire history into one frame on every connect or lag resync.
const SNAPSHOT_RECENT_LIMIT: usize = 200;

/// Provider readiness, computed off the async runtime (it reads the database).
async fn provider_views(state: &DockState) -> Vec<ProviderView> {
    let pipeline = state.pipeline.clone();
    match state
        .handle
        .spawn_blocking(move || pipeline.provider_statuses())
        .await
    {
        Ok(statuses) => statuses.iter().map(provider_view).collect(),
        Err(err) => {
            warn!(%err, "failed to collect provider statuses");
            Vec::new()
        }
    }
}

async fn snapshot(state: &DockState) -> ServerEvent {
    let mut terminal = 0usize;
    let messages = state
        .pipeline
        .snapshots()
        .await
        .iter()
        .map(message_view)
        .filter(|view| {
            if view.state.is_pending() {
                true
            } else {
                terminal += 1;
                terminal <= SNAPSHOT_RECENT_LIMIT
            }
        })
        .collect();
    ServerEvent::Snapshot {
        playback: playback_view(&state.pipeline.playback_state().await),
        volume: state.pipeline.output_volume().await.unwrap_or(1.0),
        messages,
        chat: state.chat.view(),
        providers: provider_views(state).await,
    }
}

async fn send(socket: &mut WebSocket, event: &ServerEvent) -> bool {
    match serde_json::to_string(event) {
        Ok(json) => socket.send(Message::Text(json.into())).await.is_ok(),
        Err(err) => {
            warn!(%err, "failed to encode dock event");
            true
        }
    }
}

async fn session(mut socket: WebSocket, state: Arc<DockState>) {
    // Subscribe before taking the snapshot so nothing is missed in between.
    let mut events = state.pipeline.subscribe();
    let mut chat_changes = state.chat.subscribe();
    if !send(&mut socket, &snapshot(&state).await).await {
        return;
    }
    debug!("dock client connected");

    // Slow command results (provider probes) come back through this channel so
    // the event loop never stalls.
    let (deferred_tx, mut deferred_rx) = mpsc::unbounded_channel::<ServerEvent>();
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { break };
                let Message::Text(text) = message else { continue };
                for event in handle_command(&state, &deferred_tx, &text).await {
                    if !send(&mut socket, &event).await {
                        return;
                    }
                }
            }
            deferred = deferred_rx.recv() => {
                // The sender half lives in this function; recv never fails.
                if let Some(event) = deferred && !send(&mut socket, &event).await {
                    break;
                }
            }
            event = events.recv() => {
                let outgoing = match event {
                    Ok(PipelineEvent::SnapshotUpdated(snapshot)) => Some(ServerEvent::Message {
                        message: message_view(&snapshot),
                    }),
                    Ok(PipelineEvent::PlaybackStateChanged(playback)) => Some(ServerEvent::Playback {
                        playback: playback_view(&playback),
                    }),
                    Ok(PipelineEvent::ProviderStatusesChanged(statuses)) => Some(ServerEvent::Providers {
                        providers: statuses.iter().map(provider_view).collect(),
                    }),
                    Ok(PipelineEvent::VolumeChanged(volume)) => Some(ServerEvent::Volume { volume }),
                    Ok(PipelineEvent::Cleared) => Some(ServerEvent::Cleared),
                    Ok(_) => None,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Resync with a fresh snapshot rather than guessing.
                        Some(snapshot(&state).await)
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                if let Some(event) = outgoing && !send(&mut socket, &event).await {
                    break;
                }
            }
            changed = chat_changes.changed() => {
                if changed.is_err() {
                    break;
                }
                let chat = chat_changes.borrow_and_update().clone();
                if !send(&mut socket, &ServerEvent::Chat { chat }).await {
                    break;
                }
            }
        }
    }
    debug!("dock client disconnected");
}

/// Apply one client command; returns the events to send back immediately.
async fn handle_command(
    state: &Arc<DockState>,
    deferred: &mpsc::UnboundedSender<ServerEvent>,
    text: &str,
) -> Vec<ServerEvent> {
    let command: ClientCommand = match serde_json::from_str(text) {
        Ok(command) => command,
        Err(err) => {
            return vec![ServerEvent::Error {
                message: format!("bad command: {err}"),
            }];
        }
    };
    info!(?command, "dock command");
    let pipeline = &state.pipeline;
    let result = match command {
        ClientCommand::Pause => pipeline.set_playback_enabled(false).await,
        ClientCommand::Resume => match pipeline.set_playback_enabled(true).await {
            Ok(()) => pipeline.ensure_all().await,
            Err(err) => Err(err),
        },
        ClientCommand::Stop => pipeline.stop_now().await,
        ClientCommand::Clear => pipeline.clear_all().await,
        ClientCommand::Skip { id } => pipeline.skip_message(id).await,
        ClientCommand::Replay { id } => pipeline.retry_message(id).await,
        ClientCommand::PlayNext { id } => pipeline.pin_next(id).await,
        ClientCommand::SetVolume { volume } => {
            // The pipeline broadcasts VolumeChanged, so every connected dock
            // session (not just this one) picks the new value up.
            return match pipeline.set_output_volume(volume).await {
                Ok(_) => Vec::new(),
                Err(err) => vec![ServerEvent::Error {
                    message: err.to_string(),
                }],
            };
        }
        ClientCommand::Submit { text } => {
            let trimmed = text.trim().to_string();
            if trimmed.is_empty() {
                return vec![ServerEvent::Error {
                    message: "message text is empty".to_string(),
                }];
            }
            match pipeline.submit_manual_message("Operator", trimmed).await {
                Ok(_) => pipeline.ensure_all().await,
                Err(err) => Err(err),
            }
        }
        // Chat commands answer through the supervisor's watch channel: every
        // session (not just this one) picks the change up in its select loop.
        ClientCommand::TwitchLogin { open_browser } => {
            state.chat.begin_login(open_browser);
            return Vec::new();
        }
        ClientCommand::TwitchLogout => {
            state.chat.logout();
            return Vec::new();
        }
        ClientCommand::SetChannel { channel } => {
            state.chat.set_channel_login(&channel);
            return Vec::new();
        }
        ClientCommand::SetSecret { secret, value } => {
            let value = value.trim().to_string();
            if value.is_empty() {
                return vec![ServerEvent::Error {
                    message: "secret value is empty".to_string(),
                }];
            }
            let settings = state.settings.clone();
            let key = secret_store_key(secret);
            let stored = state
                .handle
                .spawn_blocking(move || {
                    lupo_secrets::store_secret(&settings, key, value.as_bytes())
                })
                .await
                .map_err(|err| err.to_string())
                .and_then(|result| result.map_err(|err| err.to_string()));
            return match stored {
                Ok(()) => {
                    info!(?secret, "secret stored via dock");
                    // Broadcast the new provider readiness to every session.
                    let _ = state.pipeline.refresh_provider_statuses().await;
                    Vec::new()
                }
                Err(message) => vec![ServerEvent::Error { message }],
            };
        }
        ClientCommand::ForgetSecret { secret } => {
            let settings = state.settings.clone();
            let key = secret_store_key(secret);
            let deleted = state
                .handle
                .spawn_blocking(move || lupo_secrets::delete_secret(&settings, key))
                .await
                .map_err(|err| err.to_string())
                .and_then(|result| result.map_err(|err| err.to_string()));
            return match deleted {
                Ok(()) => {
                    info!(?secret, "secret removed via dock");
                    let _ = state.pipeline.refresh_provider_statuses().await;
                    Vec::new()
                }
                Err(message) => vec![ServerEvent::Error { message }],
            };
        }
        ClientCommand::Probe { provider } => {
            let state = state.clone();
            let deferred = deferred.clone();
            state.handle.clone().spawn(async move {
                let event = match state.pipeline.probe_provider(&provider, PROBE_TEXT).await {
                    Ok(probe) => ServerEvent::Probe {
                        provider: probe.provider_id,
                        ok: true,
                        detail: format!(
                            "{} returned {} KB of audio in {:.1} s",
                            probe.label,
                            probe.audio_bytes / 1024,
                            probe.elapsed.as_secs_f64()
                        ),
                    },
                    Err(err) => ServerEvent::Probe {
                        provider,
                        ok: false,
                        detail: err,
                    },
                };
                let _ = deferred.send(event);
                let _ = deferred.send(providers_delta(&state).await);
            });
            return Vec::new();
        }
    };
    match result {
        Ok(()) => Vec::new(),
        Err(err) => vec![ServerEvent::Error {
            message: err.to_string(),
        }],
    }
}

async fn providers_delta(state: &DockState) -> ServerEvent {
    ServerEvent::Providers {
        providers: provider_views(state).await,
    }
}

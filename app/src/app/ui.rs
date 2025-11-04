// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;
#[cfg(feature = "renderer-webview")]
use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
};

use dioxus::document;
use dioxus::html::{ScrollBehavior, ScrollLogicalPosition, ScrollToOptions};
use dioxus::prelude::*;
#[cfg(all(
    feature = "renderer-webview",
    any(target_os = "windows", target_os = "linux", target_os = "macos")
))]
use dioxus_desktop::use_muda_event_handler;
#[cfg(feature = "renderer-webview")]
use dioxus_desktop::wry::http::{Response as HttpResponse, StatusCode};
#[cfg(feature = "renderer-webview")]
use dioxus_desktop::{
    AssetRequest, HotKeyState, RequestAsyncResponder, use_asset_handler, use_global_shortcut,
};
#[cfg(feature = "renderer-webview")]
use directories::BaseDirs;
use lupo_configuration::{SettingDomain, SettingSource, SettingValueKind};
#[cfg(feature = "renderer-webview")]
use lupo_persistence::app_project_dirs;
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
#[cfg(feature = "renderer-webview")]
use tracing::warn;

use super::action::{
    AppAction, MENU_ITEM_OPEN_MANUAL, MENU_ITEM_OPEN_SETTINGS, MENU_ITEM_OPEN_SHORTCUTS,
    MENU_ITEM_TOGGLE_HELP_MODE, ShortcutAction, UiAction, UiEvent,
};
use super::model::AppModel;
use super::state::{
    AppState, ChatterEntry, ChatterOverrideFilter, ChatterRoleFilter, MessageDisplaySegment,
    MessageEntry, MessageStatus, Tab,
};
use crate::runtime::pipeline::{RuntimeTtsProviderState, RuntimeTtsProviderStatus};
use crate::runtime::twitch::TwitchStatus;
use crate::runtime::youtube::YouTubeStatus;

#[cfg(feature = "renderer-webview")]
const EMOTE_ASSET_ROUTE: &str = "emote";
#[cfg(feature = "renderer-webview")]
const EMOTE_CACHE_SUBDIR: &str = "emotes";

#[component]
pub fn AppRoot() -> Element {
    let mut model = use_signal(AppModel::new);

    #[cfg(feature = "renderer-webview")]
    {
        use_asset_handler(EMOTE_ASSET_ROUTE, move |request, responder| {
            serve_emote_asset_request(request, responder);
        });

        let mut shortcut_model = model;
        let _ = use_global_shortcut("CmdOrCtrl+Comma", move |state| {
            if matches!(state, HotKeyState::Pressed) {
                shortcut_model
                    .write()
                    .dispatch(AppAction::Shortcut(ShortcutAction::OpenSettings));
            }
        });
    }

    #[cfg(all(
        feature = "renderer-webview",
        any(target_os = "windows", target_os = "linux", target_os = "macos")
    ))]
    {
        let mut menu_model = model;
        let _ = use_muda_event_handler(move |event| match event.id().as_ref() {
            MENU_ITEM_OPEN_SETTINGS => menu_model
                .write()
                .dispatch(AppAction::Shortcut(ShortcutAction::OpenSettings)),
            MENU_ITEM_TOGGLE_HELP_MODE => menu_model
                .write()
                .dispatch(AppAction::Shortcut(ShortcutAction::ToggleHelpMode)),
            MENU_ITEM_OPEN_SHORTCUTS => menu_model.write().dispatch(AppAction::OpenShortcutCatalog),
            MENU_ITEM_OPEN_MANUAL => menu_model.write().dispatch(AppAction::ManualNotImplemented),
            _ => {}
        });
    }

    {
        let mut poll_model = model;
        use_future(move || async move {
            let mut poll_delay = Duration::from_millis(100);
            loop {
                tokio::time::sleep(poll_delay).await;
                let needs_poll = {
                    let app = poll_model.read();
                    app.needs_runtime_poll()
                };

                let should_quit = if needs_poll {
                    let mut app = poll_model.write();
                    app.dispatch(AppAction::Event(UiEvent::PollRuntime));
                    app.take_should_quit()
                } else {
                    false
                };

                if should_quit {
                    std::process::exit(0);
                }

                poll_delay = if needs_poll {
                    Duration::from_millis(16)
                } else {
                    Duration::from_millis(100)
                };
            }
        });
    }

    let state = {
        let app = model.read();
        Snapshot::from(app.state())
    };

    rsx! {
        style {
            r#"
            html, body {{
                margin: 0;
                padding: 0;
                width: 100%;
                height: 100%;
                overflow: hidden;
                background: #060d19;
            }}

            #main {{
                width: 100%;
                height: 100%;
            }}

            #__dx-toast[aria-hidden=\"true\"] {{
                pointer-events: none;
            }}
            "#
        }
        div {
            id: "main-shell",
            tabindex: "0",
            onmounted: move |event| async move {
                let _ = event.data().set_focus(true).await;
            },
            onkeydown: move |event| {
                if let Some(shortcut) = map_shortcut(&event) {
                    model.write().dispatch(AppAction::Shortcut(shortcut));
                }
            },
            style: shell_style(),
            nav {
                style: "display:flex; align-items:center; gap:10px; padding:12px 16px; border-bottom:1px solid var(--line); background:rgba(7,12,24,0.82); backdrop-filter: blur(6px); flex:0 0 auto; position:sticky; top:0; z-index:20;",
                {tab_button(model, state.active_tab, Tab::ChatLogs, "Chat Logs")}
                {tab_button(model, state.active_tab, Tab::Chatters, "Chatters")}
                {tab_button(model, state.active_tab, Tab::Settings, "Settings")}
                {tab_button(model, state.active_tab, Tab::Advanced, "Advanced")}
                button {
                    style: "margin-left:auto;",
                    onclick: move |_| model.write().dispatch(AppAction::RequestQuit),
                    "Quit"
                }
            }

            if state.help_mode_enabled {
                div {
                    style: "background:linear-gradient(90deg,#0f7a6d,#0b4f86); color:white; padding:8px 16px; font-size:13px; flex:0 0 auto;",
                    "Help mode enabled. Shortcuts: Cmd/Ctrl+, Settings • Space Play/Stop • Cmd/Ctrl+Shift+S Skip current • Cmd/Ctrl+Up/Down Volume • Esc Exit help mode • Cmd/Ctrl+Q Quit"
                }
            }

            div {
                style: "display:flex; flex-direction:column; flex:1; min-height:0; overflow:hidden;",
                div {
                    style: "display:flex; flex-direction:column; flex:1; min-height:0; overflow:hidden;",
                    match state.active_tab {
                        Tab::ChatLogs => rsx! { ChatLogsView { model } },
                        Tab::Chatters => rsx! { ChattersView { model } },
                        Tab::Settings => rsx! { SettingsView { model } },
                        Tab::Advanced => rsx! { AdvancedView { model } },
                    }
                }
            }

            if state.show_advanced_open_confirmation {
                div {
                    style: overlay_style(),
                    div {
                        style: modal_style(),
                        h3 { "Open Advanced Settings?" }
                        p { "Advanced keys can destabilize queue behavior and connectors. Confirm to continue." }
                        div {
                            style: "display:flex; gap:8px;",
                            button {
                                r#type: "button",
                                onclick: move |_| model.write().dispatch(AppAction::EnableAdvanced),
                                "Open advanced"
                            }
                            button {
                                r#type: "button",
                                onclick: move |_| model.write().dispatch(AppAction::CancelAdvancedOpen),
                                "Cancel"
                            }
                        }
                    }
                }
            }

            if state.show_shortcut_catalog {
                div {
                    style: overlay_style(),
                    div {
                        style: modal_style(),
                        h3 { "Shortcut catalog" }
                        p { "Cmd/Ctrl+, Settings • Space Play/Stop • Cmd/Ctrl+Shift+S Skip current • Cmd/Ctrl+Up/Down Volume • Shift+Cmd/Ctrl+/ Help mode • Esc Exit help mode • Cmd/Ctrl+Q Quit" }
                        div {
                            style: "display:flex; gap:8px;",
                            button {
                                r#type: "button",
                                onclick: move |_| model.write().dispatch(AppAction::CloseShortcutCatalog),
                                "Close"
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(feature = "renderer-webview")]
fn serve_emote_asset_request(request: AssetRequest, responder: RequestAsyncResponder) {
    let request_path = request.uri().path().to_string();
    let Some(path) = resolve_emote_asset_path(request.uri().path()) else {
        warn!(path = %request_path, "emote asset request rejected: invalid path");
        eprintln!("emote asset invalid path: {}", request_path);
        responder.respond(text_response(StatusCode::BAD_REQUEST, "invalid emote path"));
        return;
    };

    match fs::read(&path) {
        Ok(bytes) => {
            responder.respond(
                HttpResponse::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", emote_content_type(&path))
                    .header("Cache-Control", "public, max-age=31536000, immutable")
                    .body(bytes)
                    .unwrap_or_else(|_| {
                        text_response(StatusCode::INTERNAL_SERVER_ERROR, "response build error")
                    }),
            );
        }
        Err(err) => {
            warn!(
                path = %path.display(),
                request_path = %request_path,
                "emote asset read failed: {err}"
            );
            eprintln!(
                "emote asset 404 request_path={} path={} err={err}",
                request_path,
                path.display()
            );
            responder.respond(text_response(StatusCode::NOT_FOUND, "emote not found"));
        }
    }
}

#[cfg(feature = "renderer-webview")]
fn resolve_emote_asset_path(request_path: &str) -> Option<PathBuf> {
    let mut segments = request_path.trim_start_matches('/').split('/');
    let route = segments.next()?;
    if route != EMOTE_ASSET_ROUTE {
        return None;
    }

    let requested = segments.next()?;
    if requested.is_empty() || segments.next().is_some() {
        return None;
    }

    if !requested
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_')
    {
        return None;
    }

    let key = requested.split('.').next().unwrap_or(requested);
    if key.is_empty() || !key.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }

    let cache_dirs = emote_cache_dirs();
    for cache_dir in &cache_dirs {
        let direct = cache_dir.join(requested);
        if direct.is_file() {
            return Some(direct);
        }

        if let Some(path) = find_cached_emote_by_key(cache_dir, key) {
            return Some(path);
        }
    }

    warn!(
        request_path,
        key,
        cache_dirs = ?cache_dirs,
        "emote asset cache miss"
    );
    eprintln!(
        "emote asset cache miss request_path={} key={} dirs={:?}",
        request_path, key, cache_dirs
    );
    None
}

#[cfg(feature = "renderer-webview")]
fn emote_cache_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Ok(project_dirs) = app_project_dirs() {
        dirs.push(project_dirs.cache_dir().join(EMOTE_CACHE_SUBDIR));
    }

    if let Ok(data_dir) = env::var("LUPO_DATA_DIR") {
        dirs.push(PathBuf::from(data_dir).join(EMOTE_CACHE_SUBDIR));
    }

    if let Some(base_dirs) = BaseDirs::new() {
        let cache_root = base_dirs.cache_dir();
        for candidate in ["dev.sugoijan.lupo-tts", "lupo", "lupo-tts", "lupo_tts"] {
            dirs.push(cache_root.join(candidate).join(EMOTE_CACHE_SUBDIR));
        }
    }

    let mut deduped = Vec::new();
    let mut seen = HashSet::new();
    for path in dirs {
        if seen.insert(path.clone()) {
            deduped.push(path);
        }
    }

    deduped
}

#[cfg(feature = "renderer-webview")]
fn find_cached_emote_by_key(cache_dir: &Path, key: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(cache_dir).ok()?;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name.starts_with(key) && file_name[key.len()..].starts_with('.') {
            return Some(entry.path());
        }
    }
    None
}

#[cfg(feature = "renderer-webview")]
fn emote_content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("avif") => "image/avif",
        _ => "application/octet-stream",
    }
}

#[cfg(feature = "renderer-webview")]
fn text_response(status: StatusCode, message: &'static str) -> HttpResponse<Vec<u8>> {
    HttpResponse::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(message.as_bytes().to_vec())
        .unwrap_or_else(|_| HttpResponse::new(b"response build failure".to_vec()))
}

#[derive(Clone)]
struct Snapshot {
    active_tab: Tab,
    help_mode_enabled: bool,
    show_advanced_open_confirmation: bool,
    show_shortcut_catalog: bool,
}

impl From<&AppState> for Snapshot {
    fn from(state: &AppState) -> Self {
        Self {
            active_tab: state.active_tab,
            help_mode_enabled: state.help_mode_enabled,
            show_advanced_open_confirmation: state.show_advanced_open_confirmation,
            show_shortcut_catalog: state.show_shortcut_catalog,
        }
    }
}

#[component]
fn ChatLogsView(model: Signal<AppModel>) -> Element {
    let state = {
        let app = model.read();
        app.state().clone_for_chat_logs()
    };

    let queued = state
        .messages
        .iter()
        .filter(|entry| {
            matches!(
                entry.status,
                MessageStatus::Arrived
                    | MessageStatus::Transforming
                    | MessageStatus::Transformed
                    | MessageStatus::Synthesizing
                    | MessageStatus::Synthesized
                    | MessageStatus::Queued
            )
        })
        .count();
    let playing = state
        .messages
        .iter()
        .filter(|entry| matches!(entry.status, MessageStatus::Playing))
        .count();
    let latest_message_anchor_key = state.messages.last().map(|entry| entry.id).unwrap_or(0);
    let has_external_chat_connected = matches!(state.twitch_status, TwitchStatus::Connected { .. })
        || matches!(state.youtube_status, YouTubeStatus::Connected { .. });

    let (twitch_label, twitch_hint) = match &state.twitch_status {
        TwitchStatus::Idle | TwitchStatus::Connecting => (
            "Twitch connecting".to_string(),
            "Waiting for EventSub session".to_string(),
        ),
        TwitchStatus::Connected { channel } => (
            format!("Twitch connected: {channel}"),
            "Live ingestion active".to_string(),
        ),
        TwitchStatus::CredentialsMissing => (
            "Twitch credentials missing".to_string(),
            "Configure credentials from Settings -> Connections".to_string(),
        ),
        TwitchStatus::Error(message) => {
            (format!("Twitch error: {message}"), "Check logs".to_string())
        }
        TwitchStatus::Disconnected => (
            "Twitch disconnected".to_string(),
            "Listener stopped unexpectedly".to_string(),
        ),
    };

    let (youtube_label, youtube_hint) = match &state.youtube_status {
        YouTubeStatus::Idle | YouTubeStatus::Connecting => (
            "YouTube connecting".to_string(),
            "Waiting for live chat poll".to_string(),
        ),
        YouTubeStatus::Connected { live_chat_id } => (
            format!("YouTube connected: {live_chat_id}"),
            "Live ingestion active".to_string(),
        ),
        YouTubeStatus::CredentialsMissing => (
            "YouTube credentials missing".to_string(),
            "Configure live chat ID and API key in Settings -> Connections".to_string(),
        ),
        YouTubeStatus::Error(message) => (
            format!("YouTube error: {message}"),
            "Check logs".to_string(),
        ),
        YouTubeStatus::Disconnected => (
            "YouTube disconnected".to_string(),
            "Listener stopped unexpectedly".to_string(),
        ),
    };

    rsx! {
        section {
            style: "display:flex; flex-direction:column; min-height:0; flex:1; height:100%; overflow:hidden; padding:12px; gap:10px; box-sizing:border-box;",

            if !has_external_chat_connected {
                div {
                    style: "display:flex; align-items:center; gap:10px; border:1px solid #b9873f; background:rgba(146,97,31,0.16); border-radius:10px; padding:10px 12px;",
                    strong { "No external chat providers connected." }
                    span { style: "color:var(--muted);", "Local injection is active. Configure Twitch/YouTube in Settings." }
                    button {
                        style: "margin-left:auto;",
                        onclick: move |_| model.write().dispatch(AppAction::Shortcut(ShortcutAction::OpenSettings)),
                        "Open Settings"
                    }
                }
            }

            div {
                style: "display:flex; gap:12px; align-items:center; background:rgba(16,28,52,0.7); border:1px solid var(--line); border-radius:10px; padding:10px 12px; flex:0 0 auto;",
                strong { "Status" }
                span { "{twitch_label}" }
                span { style: "color:var(--muted);", "{twitch_hint}" }
                span { "{youtube_label}" }
                span { style: "color:var(--muted);", "{youtube_hint}" }
                for provider in state.provider_statuses.iter() {
                    {provider_status_chip(provider)}
                }
                span { style: "margin-left:auto; color:var(--muted);", "Queued: {queued} • Playing: {playing} • Total: {state.messages.len()}" }
            }

            if state.help_mode_enabled {
                div {
                    style: "display:flex; flex-direction:column; gap:4px; border:1px dashed #6a7fa6; border-radius:10px; padding:10px; background:rgba(7,15,29,0.72); color:var(--muted); font-size:12px;",
                    strong { style: "color:var(--text);", "Contextual help" }
                    span { "Select any row to target replay/skip/copy actions." }
                    span { "Use Cmd/Ctrl+Shift+S to skip the active playing message." }
                    span { "Use Cmd/Ctrl+Up/Down to adjust output gain." }
                }
            }

            div {
                style: "display:flex; flex-direction:column; min-height:0; flex:1 1 auto; overflow-y:auto; overflow-x:hidden; border:1px solid var(--line); border-radius:12px; background:rgba(8,16,34,0.76);",
                onscroll: move |event| {
                    let scroll = event.data();
                    let distance_from_bottom = scroll.scroll_height() as f64
                        - (scroll.scroll_top() + scroll.client_height() as f64);
                    let at_bottom = distance_from_bottom <= 8.0;
                    model
                        .write()
                        .dispatch(AppAction::ChatLogScrollPositionChanged { at_bottom });
                },
                if state.messages.is_empty() {
                    div {
                        style: "display:flex; flex-direction:column; gap:10px; padding:24px; color:var(--muted);",
                        "No queued messages yet."
                        if !has_external_chat_connected {
                            div {
                                style: "display:flex; flex-direction:column; gap:8px; padding:12px; border:1px solid var(--line); border-radius:10px; background:rgba(7,15,29,0.72);",
                                strong { style: "color:var(--text);", "Onboarding" }
                                span { "Connect Twitch or YouTube in Settings to ingest live chat. You can still inject local test messages below." }
                                button {
                                    style: "max-width:160px;",
                                    onclick: move |_| model.write().dispatch(AppAction::Shortcut(ShortcutAction::OpenSettings)),
                                    "Open Settings"
                                }
                            }
                        }
                    }
                } else {
                    for entry in state.messages.iter() {
                        {message_row(model, entry.clone(), state.active_playing_id, state.selected_message_id, state.help_mode_enabled)}
                    }
                    if state.sticky_to_latest {
                        div {
                            key: "chat-log-anchor-{latest_message_anchor_key}",
                            style: "height:1px; min-height:1px;",
                            onmounted: move |event| async move {
                                let _ = event
                                    .data()
                                    .scroll_to_with_options(ScrollToOptions {
                                        behavior: ScrollBehavior::Instant,
                                        vertical: ScrollLogicalPosition::End,
                                        horizontal: ScrollLogicalPosition::Nearest,
                                    })
                                    .await;
                            },
                        }
                    }
                }
            }

            div {
                style: "display:flex; flex-direction:column; gap:10px; border:1px solid var(--line); border-radius:10px; background:rgba(10,22,42,0.78); padding:12px; flex:0 0 auto;",
                div { style: "font-weight:600;", "Playback strip" }
                div {
                    style: "display:flex; gap:8px; align-items:stretch; flex-wrap:wrap;",
                    input {
                        style: "flex:1 1 420px; min-width:280px; height:42px; padding:0 12px; line-height:42px; caret-color:var(--accent);",
                        r#type: "text",
                        placeholder: "Inject message",
                        value: "{state.input_value}",
                        oninput: move |event| {
                            model.write().dispatch(AppAction::InputChanged(event.value()));
                        },
                        onkeydown: move |event| {
                            if event.key().to_string() == "Enter" {
                                model.write().dispatch(AppAction::Trigger(UiAction::QueueMessage));
                            }
                        }
                    }
                    button {
                        disabled: state.busy || state.input_value.trim().is_empty(),
                        onclick: move |_| model.write().dispatch(AppAction::Trigger(UiAction::QueueMessage)),
                        "Add to queue"
                    }
                    button {
                        disabled: state.busy || state.input_value.trim().is_empty(),
                        onclick: move |_| model.write().dispatch(AppAction::Trigger(UiAction::PlayNow)),
                        "Play now"
                    }
                    button {
                        disabled: state.busy,
                        onclick: move |_| model.write().dispatch(AppAction::Trigger(UiAction::TogglePlayback)),
                        if state.playback_enabled { "Stop" } else { "Play" }
                    }
                    button {
                        disabled: state.busy || state.messages.is_empty(),
                        onclick: move |_| model.write().dispatch(AppAction::Trigger(UiAction::ClearQueue)),
                        "Clear queue"
                    }
                }
                div {
                    style: "display:flex; gap:8px; color:var(--muted); font-size:13px;",
                    span { "Volume shortcuts: Cmd/Ctrl+Up increases gain, Cmd/Ctrl+Down decreases gain." }
                    span { "Gain is persisted to audio output settings." }
                }
            }

            if let Some(status) = state.status_message {
                div {
                    style: "color:var(--accent); flex:0 0 auto; white-space:nowrap; overflow:hidden; text-overflow:ellipsis;",
                    "{status}"
                }
            }

            if !state.sticky_to_latest && state.hidden_message_count > 0 {
                div {
                    style: "color:var(--muted); flex:0 0 auto;",
                    "Hidden messages: {state.hidden_message_count}"
                }
            }

            if state.show_stop_confirmation {
                div {
                    style: overlay_style(),
                    div {
                        style: modal_style(),
                        h3 { "Stop playback?" }
                        p { "Stop now halts active playback and keeps remaining queue entries." }
                        div {
                            style: "display:flex; gap:8px;",
                            button {
                                onclick: move |_| model.write().dispatch(AppAction::StopContinue),
                                "Continue playback"
                            }
                            button {
                                onclick: move |_| model.write().dispatch(AppAction::Trigger(UiAction::StopNow)),
                                "Stop now"
                            }
                            button {
                                onclick: move |_| model.write().dispatch(AppAction::StopCancel),
                                "Cancel"
                            }
                        }
                    }
                }
            }

            if state.show_quit_confirmation {
                div {
                    style: overlay_style(),
                    div {
                        style: modal_style(),
                        h3 { "Messages still queued/playing" }
                        p { "Choose how to handle playback before quitting." }
                        div {
                            style: "display:flex; gap:8px;",
                            button {
                                onclick: move |_| model.write().dispatch(AppAction::QuitAnyway),
                                "Quit anyway"
                            }
                            button {
                                onclick: move |_| model.write().dispatch(AppAction::QuitStopBefore),
                                "Stop before quitting"
                            }
                            button {
                                onclick: move |_| model.write().dispatch(AppAction::QuitCancel),
                                "Don't quit"
                            }
                        }
                    }
                }
            }
        }
    }
}

fn message_row(
    mut model: Signal<AppModel>,
    entry: MessageEntry,
    active_playing_id: Option<u64>,
    selected_message_id: Option<u64>,
    help_mode_enabled: bool,
) -> Element {
    let status = entry.status_badge();
    let source_timestamp = entry.source_timestamp.unwrap_or(entry.received_at);
    let timestamp = source_timestamp
        .format(&Rfc3339)
        .unwrap_or_else(|_| OffsetDateTime::now_utc().to_string());
    let received_timestamp = entry
        .received_at
        .format(&Rfc3339)
        .unwrap_or_else(|_| OffsetDateTime::now_utc().to_string());
    let is_playing = active_playing_id == Some(entry.id);
    let is_selected = selected_message_id == Some(entry.id);
    let message_id = entry.id;
    let copy_text_payload = entry.display_text.clone();
    let profile_link_payload = entry.profile_link.clone();
    let sender_style = match entry.sender_color {
        Some(ref color) => format!("font-weight:600; color:{color};"),
        None => "font-weight:600;".to_string(),
    };

    rsx! {
        article {
            key: "{entry.id}",
            style: format!(
                "padding:10px 12px; border-bottom:1px solid var(--line); background:{}; border-left:3px solid {};",
                if is_playing { "rgba(20,76,57,0.35)" } else { "transparent" },
                if is_selected { "var(--accent)" } else { "transparent" }
            ),
            onclick: move |_| model.write().dispatch(AppAction::ChatRowSelected(message_id)),
            div {
                style: "display:flex; gap:8px; align-items:center;",
                span { style: "font-weight:600;", "{entry.provider.label()}" }
                span { style: "{sender_style}", "{entry.sender}" }
                for badge in entry.badges.iter() {
                    span {
                        style: "font-size:11px; border:1px solid var(--line); border-radius:999px; padding:2px 6px; color:var(--muted);",
                        "{badge}"
                    }
                }
                span { style: "margin-left:auto; color:var(--muted);", "{status}" }
            }
            div {
                style: "padding-top:4px; display:flex; flex-wrap:wrap; gap:4px; align-items:center;",
                for segment in entry.display_segments.iter() {
                    match segment {
                        MessageDisplaySegment::Text(text) => rsx! { span { "{text}" } },
                        MessageDisplaySegment::Emote { code, url } => rsx! {
                            if let Some(url) = url {
                                img {
                                    src: "{url}",
                                    alt: "{code}",
                                    title: "{code}",
                                    style: "height:1.2em; width:auto; max-width:3em; object-fit:contain; vertical-align:middle;",
                                }
                            } else {
                                span {
                                    style: "font-size:12px; border:1px solid var(--line); border-radius:6px; padding:1px 4px;",
                                    "{code}"
                                }
                            }
                        },
                    }
                }
            }
            if let Some(trace) = &entry.synthesis_trace {
                div {
                    style: "padding-top:2px; color:var(--muted); font-size:12px;",
                    "{trace}"
                }
            }
            div { style: "padding-top:2px; color:var(--muted); font-size:12px;", title: "source: {timestamp} • received: {received_timestamp}", "#{entry.id} • {timestamp}" }
            div {
                style: "display:flex; flex-wrap:wrap; gap:8px; margin-top:8px;",
                button {
                    r#type: "button",
                    title: if help_mode_enabled { "Copy rendered message text to clipboard." } else { "" },
                    onclick: move |_| {
                        copy_text_to_clipboard(&copy_text_payload);
                        model.write().dispatch(AppAction::SetStatusMessage(format!("Copied message #{id}.", id = message_id)));
                    },
                    "Copy text"
                }
                button {
                    r#type: "button",
                    title: if help_mode_enabled { "Requeue this message for synthesis/playback." } else { "" },
                    onclick: move |_| model.write().dispatch(AppAction::ReplayMessage(message_id)),
                    "Replay"
                }
                button {
                    r#type: "button",
                    title: if help_mode_enabled { "Skip this message in queue or playback." } else { "" },
                    onclick: move |_| model.write().dispatch(AppAction::SkipMessage(message_id)),
                    "Skip"
                }
                button {
                    r#type: "button",
                    title: if help_mode_enabled { "Copy source profile/context URL when available." } else { "" },
                    onclick: move |_| {
                        if let Some(link) = &profile_link_payload {
                            copy_text_to_clipboard(link);
                            model.write().dispatch(AppAction::SetStatusMessage(format!("Copied profile/context link for #{id}.", id = message_id)));
                        } else {
                            model.write().dispatch(AppAction::SetStatusMessage("No profile link available for this message source.".to_string()));
                        }
                    },
                    "Copy profile link"
                }
            }
        }
    }
}

fn provider_status_chip(status: &RuntimeTtsProviderStatus) -> Element {
    let (state_label, state_style) = match status.state {
        RuntimeTtsProviderState::Ready => (
            "ready",
            "border-color:rgba(72,195,167,0.55); color:#9de7d7;",
        ),
        RuntimeTtsProviderState::Unavailable => (
            "unavailable",
            "border-color:rgba(160,176,205,0.42); color:#c3d0e6;",
        ),
        RuntimeTtsProviderState::Error => {
            ("error", "border-color:rgba(220,89,89,0.65); color:#ffb4b4;")
        }
    };
    let label = format!("{} [{state_label}]", status.label);

    rsx! {
        span {
            title: status.detail.clone().unwrap_or_default(),
            style: format!(
                "font-size:12px; padding:3px 8px; border:1px solid; border-radius:999px; background:rgba(6,12,24,0.65); {state_style}"
            ),
            "{label}"
        }
    }
}

#[component]
fn ChattersView(model: Signal<AppModel>) -> Element {
    let state = {
        let app = model.read();
        app.state().clone_for_chatters()
    };

    let mut filtered: Vec<ChatterEntry> = state.chatters.clone();
    let search = state.chatter_search_draft.trim().to_ascii_lowercase();
    if !search.is_empty() {
        filtered.retain(|entry: &ChatterEntry| {
            entry
                .effective_display_name()
                .to_ascii_lowercase()
                .contains(&search)
                || entry.sender.to_ascii_lowercase().contains(&search)
                || entry
                    .chatter_login
                    .as_ref()
                    .map(|login: &String| login.to_ascii_lowercase().contains(&search))
                    .unwrap_or(false)
        });
    }
    match state.chatter_role_filter {
        ChatterRoleFilter::All => {}
        ChatterRoleFilter::Moderators => filtered.retain(|entry: &ChatterEntry| entry.is_moderator),
        ChatterRoleFilter::ChannelOwner => {
            filtered.retain(|entry: &ChatterEntry| entry.is_channel_owner)
        }
    }
    match state.chatter_override_filter {
        ChatterOverrideFilter::All => {}
        ChatterOverrideFilter::HasVoiceOverride => {
            filtered.retain(ChatterEntry::has_voice_override)
        }
        ChatterOverrideFilter::NoVoiceOverride => {
            filtered.retain(|entry: &ChatterEntry| !entry.has_voice_override())
        }
    }

    rsx! {
        section {
            style: "display:flex; gap:12px; padding:16px; flex:1 1 auto; min-height:0; height:100%; overflow:hidden; box-sizing:border-box;",
            div {
                style: "display:flex; flex-direction:column; gap:10px; min-width:360px; max-width:420px; flex:0 0 36%; border:1px solid var(--line); border-radius:12px; padding:12px; background:rgba(9,16,33,0.75);",
                h2 { "Chatters" }
                p { style: "margin:0; color:var(--muted);", "Per-user voice/display overrides and moderation notes." }
                if state.help_mode_enabled {
                    p { style: "margin:0; color:var(--muted); font-size:12px;", "Select a chatter to inspect profile metadata and save overrides." }
                }
                input {
                    r#type: "text",
                    placeholder: "Search by chatter, login, or display name",
                    value: "{state.chatter_search_draft}",
                    oninput: move |event| model
                        .write()
                        .dispatch(AppAction::ChatterSearchDraftChanged(event.value())),
                }
                div {
                    style: "display:flex; gap:8px;",
                    select {
                        value: "{chatter_role_filter_value(state.chatter_role_filter)}",
                        onchange: move |event| model
                            .write()
                            .dispatch(AppAction::ChatterRoleFilterChanged(event.value())),
                        option { value: "all", "All roles" }
                        option { value: "moderators", "Moderators" }
                        option { value: "channel_owner", "Channel owner" }
                    }
                    select {
                        value: "{chatter_override_filter_value(state.chatter_override_filter)}",
                        onchange: move |event| model
                            .write()
                            .dispatch(AppAction::ChatterOverrideFilterChanged(event.value())),
                        option { value: "all", "All overrides" }
                        option { value: "has_voice_override", "Has voice override" }
                        option { value: "no_voice_override", "No voice override" }
                    }
                }
                div {
                    style: "display:flex; flex-direction:column; gap:8px; overflow-y:auto; min-height:0; flex:1 1 auto;",
                    if filtered.is_empty() {
                        p { style: "color:var(--muted);", "No chatters match the current filters." }
                    } else {
                        for chatter in filtered.iter().cloned() {
                            button {
                                key: "{chatter.chatter_id}",
                                style: format!(
                                    "text-align:left; display:flex; flex-direction:column; gap:4px; border:1px solid var(--line); border-radius:10px; padding:10px; background:{};",
                                    if state.selected_chatter_id.as_deref() == Some(chatter.chatter_id.as_str()) {
                                        "rgba(14,46,64,0.78)"
                                    } else {
                                        "rgba(6,14,26,0.7)"
                                    }
                                ),
                                onclick: move |_| model.write().dispatch(AppAction::ChatterSelected(chatter.chatter_id.clone())),
                                strong { "{chatter.effective_display_name()}" }
                                p {
                                    style: "margin:0; color:var(--muted); font-size:12px;",
                                    "{chatter.provider_id} • msgs {chatter.message_count} • last seen {format_timestamp(chatter.last_seen_at)}"
                                }
                                if chatter.has_voice_override() {
                                    span { style: "font-size:12px; color:var(--accent);", "Voice override active" }
                                }
                            }
                        }
                    }
                }
            }

            div {
                style: "display:flex; flex-direction:column; gap:10px; flex:1 1 auto; min-width:0; border:1px solid var(--line); border-radius:12px; padding:12px; background:rgba(9,16,33,0.75); overflow-y:auto;",
                h3 { "Selected chatter" }
                if let Some(chatter) = selected_chatter(state.selected_chatter_id.as_deref(), &state.chatters).cloned() {
                    p { style: "margin:0; color:var(--muted);", "Provider: {chatter.provider_id} • Canonical ID: {chatter.chatter_id}" }
                    p { style: "margin:0; color:var(--muted);", "Badges: {badges_label(&chatter.badges)}" }
                    p { style: "margin:0; color:var(--muted);", "Message count: {chatter.message_count}" }
                    if let Some(link) = chatter.profile_link.clone() {
                        div {
                            style: "display:flex; gap:8px; align-items:center;",
                            span { style: "color:var(--muted);", "Profile/context link:" }
                            code { "{link}" }
                            button {
                                r#type: "button",
                                onclick: move |_| {
                                    copy_text_to_clipboard(&link);
                                    model.write().dispatch(AppAction::SetStatusMessage("Copied chatter profile link.".to_string()));
                                },
                                "Copy link"
                            }
                        }
                    }
                    label { "Voice override" }
                    input {
                        r#type: "text",
                        placeholder: "e.g. nova",
                        value: "{state.chatter_voice_draft}",
                        oninput: move |event| model
                            .write()
                            .dispatch(AppAction::ChatterVoiceDraftChanged(event.value())),
                    }
                    label { "Display name override" }
                    input {
                        r#type: "text",
                        placeholder: "Optional display alias",
                        value: "{state.chatter_display_name_draft}",
                        oninput: move |event| model
                            .write()
                            .dispatch(AppAction::ChatterDisplayNameDraftChanged(event.value())),
                    }
                    label { "Notes" }
                    textarea {
                        style: "min-height:110px;",
                        value: "{state.chatter_notes_draft}",
                        oninput: move |event| model
                            .write()
                            .dispatch(AppAction::ChatterNotesDraftChanged(event.value())),
                    }
                    div {
                        style: "display:flex; gap:8px;",
                        button {
                            r#type: "button",
                            onclick: move |_| model.write().dispatch(AppAction::SaveSelectedChatterOverride),
                            "Save override"
                        }
                        button {
                            r#type: "button",
                            onclick: move |_| model.write().dispatch(AppAction::ResetSelectedChatterOverride),
                            "Reset drafts"
                        }
                    }
                } else {
                    p { style: "color:var(--muted);", "Select a chatter to edit overrides and notes." }
                }

                if let Some(message) = &state.chatter_status_message {
                    p { style: "margin:0; color:var(--accent);", "{message}" }
                }
            }
        }
    }
}

#[component]
fn SettingsView(model: Signal<AppModel>) -> Element {
    let state = {
        let app = model.read();
        app.state().clone_for_settings()
    };

    let grouped_keys: [(&str, Option<SettingDomain>, &str); 9] = [
        (
            "Connections",
            None,
            "Provider auth/reconnect/revoke controls.",
        ),
        (
            "Message filtering",
            Some(SettingDomain::MessageFiltering),
            "Prefix, command, bot, and paid-message rules.",
        ),
        (
            "Command acceptance",
            Some(SettingDomain::CommandAcceptance),
            "Who can issue chat command actions.",
        ),
        (
            "Text transformation",
            Some(SettingDomain::TextTransformation),
            "Speech template and normalization rules.",
        ),
        (
            "TTS provider selection",
            Some(SettingDomain::Tts),
            "Provider priority and fallback.",
        ),
        (
            "Offline TTS strategy",
            Some(SettingDomain::OfflineTts),
            "Local/offline engine posture and resource guidance.",
        ),
        (
            "Voice strategy",
            Some(SettingDomain::VoiceStrategy),
            "Per-user and global voice override policy.",
        ),
        (
            "Audio output",
            Some(SettingDomain::AudioOutput),
            "Output route selection and gain policy.",
        ),
        (
            "Help and shortcuts",
            None,
            "Help-mode toggle and shortcut catalog.",
        ),
    ];

    rsx! {
        section {
            style: "display:flex; flex-direction:column; gap:12px; padding:16px; flex:1 1 auto; min-height:0; height:100%; overflow:hidden; box-sizing:border-box; position:relative; z-index:0;",
            div {
                style: "display:flex; flex-direction:column; gap:12px; min-height:0; flex:1 1 auto; overflow-y:auto; overflow-x:hidden; overscroll-behavior:contain; padding-right:2px; pointer-events:auto;",
                div {
                    style: "display:flex; align-items:center; gap:8px;",
                    h2 { "Settings" }
                    button {
                        r#type: "button",
                        onclick: move |_| model.write().dispatch(AppAction::ReloadSettings),
                        "Reload"
                    }
                    span { style: "margin-left:auto; color:var(--muted);", "Orphans: {state.orphan_count}" }
                }

                if let Some(status) = &state.settings_status_message {
                    div { style: "color:var(--accent);", "{status}" }
                }

                if let Some(error) = &state.settings_error {
                    div { style: "color:#ff9b99;", "{error}" }
                }

                for (title, domain, description) in grouped_keys {
                    div {
                        style: "border:1px solid var(--line); border-radius:12px; padding:12px; background:rgba(9,16,33,0.75);",
                        h3 { "{title}" }
                        p { style: "color:var(--muted);", "{description}" }

                        if let Some(domain) = domain {
                            for row in settings_for_domain(&state, domain) {
                                {setting_editor_card(model, row)}
                            }
                            if settings_for_domain(&state, domain).is_empty() {
                                {unsupported_card(
                                    "No matching keys",
                                    "This section is defined by DESIGN.md but has no surfaced keys in current settings schema.",
                                )}
                            }
                        } else if title == "Connections" {
                            {connections_controls(model, &state)}
                        } else if title == "Help and shortcuts" {
                            p { "Toggle help mode via Shift+Cmd+/ (macOS) or Shift+Ctrl+/." }
                            p { style: "color:var(--muted);", "Core shortcuts are handled by this shell's keyboard bindings where available." }
                        }
                    }
                }
            }
        }
    }
}

fn connections_controls(mut model: Signal<AppModel>, state: &SettingsSnapshot) -> Element {
    let (twitch_state_label, twitch_state_detail) = match &state.twitch_status {
        TwitchStatus::Idle => ("Idle", "Listener is not running.".to_string()),
        TwitchStatus::Connecting => ("Connecting", "Establishing EventSub session...".to_string()),
        TwitchStatus::Connected { channel } => (
            "Connected",
            format!("Ingestion active for channel: {channel}."),
        ),
        TwitchStatus::CredentialsMissing => (
            "Credentials missing",
            "Authenticate Twitch credentials, then reconnect.".to_string(),
        ),
        TwitchStatus::Error(message) => ("Error", message.clone()),
        TwitchStatus::Disconnected => ("Disconnected", "Listener stopped.".to_string()),
    };
    let twitch_credentials_present = setting_is_persisted(state, "secret_twitch_access_token")
        || setting_is_persisted(state, "twitch_auth_state");
    let twitch_updated = setting_updated_label(state, "secret_twitch_access_token")
        .or_else(|| setting_updated_label(state, "twitch_auth_state"));
    let twitch_login = twitch_account_login(state);
    let channel_override = connections_channel_override(state);
    let channel_override_active = channel_override.is_some();
    let can_save_channel =
        !state.twitch_channel_login_draft.trim().is_empty() || channel_override_active;
    let twitch_connecting = matches!(state.twitch_status, TwitchStatus::Connecting);

    let (youtube_state_label, youtube_state_detail) = match &state.youtube_status {
        YouTubeStatus::Idle => ("Idle", "Listener is not running.".to_string()),
        YouTubeStatus::Connecting => (
            "Connecting",
            "Polling YouTube Live Chat endpoint...".to_string(),
        ),
        YouTubeStatus::Connected { live_chat_id } => (
            "Connected",
            format!("Ingestion active for live chat: {live_chat_id}."),
        ),
        YouTubeStatus::CredentialsMissing => (
            "Credentials missing",
            "Set live chat ID and YouTube API key, then reconnect.".to_string(),
        ),
        YouTubeStatus::Error(message) => ("Error", message.clone()),
        YouTubeStatus::Disconnected => ("Disconnected", "Listener stopped.".to_string()),
    };
    let youtube_chat_id = connections_youtube_live_chat_id(state);
    let youtube_chat_id_active = youtube_chat_id.is_some();
    let can_save_youtube_chat_id =
        !state.youtube_live_chat_id_draft.trim().is_empty() || youtube_chat_id_active;
    let youtube_connecting = matches!(state.youtube_status, YouTubeStatus::Connecting);
    let youtube_key_configured = setting_is_persisted(state, "secret_youtube_api_key");
    let youtube_key_updated = setting_updated_label(state, "secret_youtube_api_key");
    let can_save_youtube_key = !state.youtube_api_key_draft.trim().is_empty();

    let openai_configured = setting_is_persisted(state, "secret_openai_api_key");
    let openai_updated = setting_updated_label(state, "secret_openai_api_key");
    let can_save_openai = !state.openai_api_key_draft.trim().is_empty();

    rsx! {
        div {
            style: "display:flex; flex-direction:column; gap:10px; margin-top:10px;",
            div {
                style: "display:flex; flex-direction:column; gap:8px; padding:10px; border:1px solid var(--line); border-radius:10px; background:rgba(5,12,24,0.7);",
                div {
                    style: "display:flex; align-items:center; gap:8px;",
                    strong { "Twitch Chat" }
                    span {
                        style: "margin-left:auto; color:var(--muted);",
                        "{twitch_state_label}"
                    }
                }
                p { style: "margin:0; color:var(--muted);", "{twitch_state_detail}" }
                if let Some(login) = &twitch_login {
                    p { style: "margin:0; color:var(--muted);", "Linked account: {login}" }
                }
                if let Some(updated) = twitch_updated {
                    p { style: "margin:0; color:var(--muted);", "Credentials updated: {updated}" }
                }
                p {
                    style: "margin:0; color:var(--muted);",
                    if let Some(override_login) = &channel_override {
                        "Target channel override: {override_login}"
                    } else if let Some(login) = &twitch_login {
                        "Target channel: {login} (connected account default)"
                    } else {
                        "Target channel: connected account default"
                    }
                }
                input {
                    r#type: "text",
                    placeholder: if let Some(login) = &twitch_login {
                        format!("Override channel login (default: {login})")
                    } else {
                        "Override channel login (optional)".to_string()
                    },
                    value: "{state.twitch_channel_login_draft}",
                    oninput: move |event| model
                        .write()
                        .dispatch(AppAction::TwitchChannelLoginDraftChanged(event.value())),
                }
                div {
                    style: "display:flex; gap:8px;",
                    button {
                        r#type: "button",
                        disabled: !can_save_channel,
                        onclick: move |_| model.write().dispatch(AppAction::SaveTwitchChannelLogin),
                        if channel_override_active { "Update target channel" } else { "Set target channel" }
                    }
                    button {
                        r#type: "button",
                        disabled: !channel_override_active,
                        onclick: move |_| model.write().dispatch(AppAction::ClearTwitchChannelLogin),
                        "Use connected account"
                    }
                    button {
                        r#type: "button",
                        disabled: twitch_connecting,
                        onclick: move |_| model.write().dispatch(AppAction::ReconnectTwitch),
                        if twitch_connecting { "Connecting..." } else { "Reconnect" }
                    }
                    button {
                        r#type: "button",
                        disabled: !twitch_credentials_present,
                        onclick: move |_| model.write().dispatch(AppAction::RevokeTwitchCredentials),
                        "Revoke credentials"
                    }
                }
            }

            div {
                style: "display:flex; flex-direction:column; gap:8px; padding:10px; border:1px solid var(--line); border-radius:10px; background:rgba(5,12,24,0.7);",
                div {
                    style: "display:flex; align-items:center; gap:8px;",
                    strong { "YouTube Live Chat" }
                    span {
                        style: "margin-left:auto; color:var(--muted);",
                        "{youtube_state_label}"
                    }
                }
                p { style: "margin:0; color:var(--muted);", "{youtube_state_detail}" }
                p {
                    style: "margin:0; color:var(--muted);",
                    if let Some(chat_id) = &youtube_chat_id {
                        "Live chat ID: {chat_id}"
                    } else {
                        "Live chat ID is not configured."
                    }
                }
                input {
                    r#type: "text",
                    placeholder: "YouTube live chat ID",
                    value: "{state.youtube_live_chat_id_draft}",
                    oninput: move |event| model
                        .write()
                        .dispatch(AppAction::YouTubeLiveChatIdDraftChanged(event.value())),
                }
                div {
                    style: "display:flex; gap:8px;",
                    button {
                        r#type: "button",
                        disabled: !can_save_youtube_chat_id,
                        onclick: move |_| model.write().dispatch(AppAction::SaveYouTubeLiveChatId),
                        if youtube_chat_id_active { "Update chat ID" } else { "Set chat ID" }
                    }
                    button {
                        r#type: "button",
                        disabled: !youtube_chat_id_active,
                        onclick: move |_| model.write().dispatch(AppAction::ClearYouTubeLiveChatId),
                        "Clear chat ID"
                    }
                    button {
                        r#type: "button",
                        disabled: youtube_connecting,
                        onclick: move |_| model.write().dispatch(AppAction::ReconnectYouTube),
                        if youtube_connecting { "Connecting..." } else { "Reconnect" }
                    }
                }
                p { style: "margin:0; color:var(--muted);", "API key is stored in the embedded secret store and never rendered back in plaintext." }
                if let Some(updated) = youtube_key_updated {
                    p { style: "margin:0; color:var(--muted);", "API key updated: {updated}" }
                }
                input {
                    r#type: "password",
                    placeholder: if youtube_key_configured { "Enter new YouTube API key" } else { "Enter YouTube API key" },
                    autocomplete: "off",
                    value: "{state.youtube_api_key_draft}",
                    oninput: move |event| model
                        .write()
                        .dispatch(AppAction::YouTubeApiKeyDraftChanged(event.value())),
                }
                div {
                    style: "display:flex; gap:8px;",
                    button {
                        r#type: "button",
                        disabled: !can_save_youtube_key,
                        onclick: move |_| model.write().dispatch(AppAction::SaveYouTubeApiKey),
                        if youtube_key_configured { "Update key" } else { "Save key" }
                    }
                    button {
                        r#type: "button",
                        disabled: !youtube_key_configured,
                        onclick: move |_| model.write().dispatch(AppAction::RevokeYouTubeApiKey),
                        "Revoke key"
                    }
                }
            }

            div {
                style: "display:flex; flex-direction:column; gap:8px; padding:10px; border:1px solid var(--line); border-radius:10px; background:rgba(5,12,24,0.7);",
                div {
                    style: "display:flex; align-items:center; gap:8px;",
                    strong { "OpenAI TTS" }
                    span {
                        style: "margin-left:auto; color:var(--muted);",
                        if openai_configured { "Configured" } else { "Missing API key" }
                    }
                }
                p { style: "margin:0; color:var(--muted);", "Token is stored in the embedded secret store and never rendered back in plaintext." }
                if let Some(updated) = openai_updated {
                    p { style: "margin:0; color:var(--muted);", "Token updated: {updated}" }
                }
                input {
                    r#type: "password",
                    placeholder: if openai_configured { "Enter new OpenAI API key" } else { "Enter OpenAI API key" },
                    autocomplete: "off",
                    value: "{state.openai_api_key_draft}",
                    oninput: move |event| model
                        .write()
                        .dispatch(AppAction::OpenAiApiKeyDraftChanged(event.value())),
                }
                div {
                    style: "display:flex; gap:8px;",
                    button {
                        r#type: "button",
                        disabled: !can_save_openai,
                        onclick: move |_| model.write().dispatch(AppAction::SaveOpenAiApiKey),
                        if openai_configured { "Update key" } else { "Save key" }
                    }
                    button {
                        r#type: "button",
                        disabled: !openai_configured,
                        onclick: move |_| model.write().dispatch(AppAction::RevokeOpenAiApiKey),
                        "Revoke key"
                    }
                }
            }
        }
    }
}

fn setting_is_persisted(state: &SettingsSnapshot, key: &str) -> bool {
    setting_for_key(state, key)
        .map(|entry| matches!(&entry.source, SettingSource::Persisted { .. }))
        .unwrap_or(false)
}

fn setting_updated_label(state: &SettingsSnapshot, key: &str) -> Option<String> {
    let entry = setting_for_key(state, key)?;
    match &entry.source {
        SettingSource::Persisted { updated_at, .. } => Some(format_timestamp(*updated_at)),
        SettingSource::Default => None,
    }
}

fn setting_for_key<'a>(
    state: &'a SettingsSnapshot,
    key: &str,
) -> Option<&'a super::state::SettingDisplay> {
    state.entries.iter().find(|entry| entry.key == key)
}

fn twitch_account_login(state: &SettingsSnapshot) -> Option<String> {
    let entry = setting_for_key(state, "twitch_auth_state")?;
    let value = serde_json::from_str::<Value>(&entry.value_json).ok()?;
    let login_state = value.get("login_state")?;
    let login = login_state.get("login")?.as_str()?;
    Some(login.to_string())
}

fn connections_channel_override(state: &SettingsSnapshot) -> Option<String> {
    let entry = setting_for_key(state, "connections")?;
    let value = serde_json::from_str::<Value>(&entry.value_json).ok()?;
    let login = value.get("twitch_channel_login")?.as_str()?;
    let trimmed = login.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_ascii_lowercase())
    }
}

fn connections_youtube_live_chat_id(state: &SettingsSnapshot) -> Option<String> {
    let entry = setting_for_key(state, "connections")?;
    let value = serde_json::from_str::<Value>(&entry.value_json).ok()?;
    let chat_id = value.get("youtube_live_chat_id")?.as_str()?;
    let trimmed = chat_id.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn format_timestamp(value: OffsetDateTime) -> String {
    value.format(&Rfc3339).unwrap_or_else(|_| value.to_string())
}

fn copy_text_to_clipboard(value: &str) {
    let escaped = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string());
    let script = format!(
        "const text = {escaped}; if (navigator && navigator.clipboard && navigator.clipboard.writeText) {{ navigator.clipboard.writeText(text); }}"
    );
    let _ = document::eval(&script);
}

fn chatter_role_filter_value(filter: ChatterRoleFilter) -> &'static str {
    match filter {
        ChatterRoleFilter::All => "all",
        ChatterRoleFilter::Moderators => "moderators",
        ChatterRoleFilter::ChannelOwner => "channel_owner",
    }
}

fn chatter_override_filter_value(filter: ChatterOverrideFilter) -> &'static str {
    match filter {
        ChatterOverrideFilter::All => "all",
        ChatterOverrideFilter::HasVoiceOverride => "has_voice_override",
        ChatterOverrideFilter::NoVoiceOverride => "no_voice_override",
    }
}

fn selected_chatter<'a>(
    selected_chatter_id: Option<&str>,
    chatters: &'a [ChatterEntry],
) -> Option<&'a ChatterEntry> {
    let selected_chatter_id = selected_chatter_id?;
    chatters
        .iter()
        .find(|chatter| chatter.chatter_id == selected_chatter_id)
}

fn badges_label(badges: &[String]) -> String {
    if badges.is_empty() {
        "none".to_string()
    } else {
        badges.join(", ")
    }
}

fn setting_editor_card(mut model: Signal<AppModel>, row: SettingRow) -> Element {
    let source_label = match row.source {
        SettingSource::Default => "default".to_string(),
        SettingSource::Persisted { scope, .. } => format!("persisted ({scope})"),
    };
    let key_for_input = row.key.clone();
    let key_for_save = row.key.clone();
    let key_for_reset = row.key.clone();
    let key_for_discard = row.key.clone();

    rsx! {
        div {
            key: "{row.key}",
            style: "display:flex; flex-direction:column; gap:8px; padding:10px; border:1px solid var(--line); border-radius:10px; background:rgba(5,12,24,0.7); margin-top:10px;",
            div {
                style: "display:flex; gap:8px;",
                strong { "{row.title}" }
                span { style: "color:var(--muted);", "{source_label}" }
            }
            p { style: "margin:0; color:var(--muted);", "{row.description}" }
            code { "{row.key} • {row.domain.as_str()}" }
            textarea {
                style: "min-height:80px;",
                value: "{row.draft_value}",
                oninput: move |event| {
                    model.write().dispatch(AppAction::SettingDraftChanged {
                        key: key_for_input.clone(),
                        draft: event.value(),
                    });
                }
            }
            div {
                style: "display:flex; gap:8px; align-items:center;",
                button {
                    r#type: "button",
                    disabled: !row.dirty,
                    onclick: move |_| model.write().dispatch(AppAction::SaveSetting(key_for_save.clone())),
                    "Save"
                }
                button {
                    r#type: "button",
                    onclick: move |_| model.write().dispatch(AppAction::ResetSetting(key_for_reset.clone())),
                    "Reset"
                }
                button {
                    r#type: "button",
                    disabled: !row.dirty,
                    onclick: move |_| model.write().dispatch(AppAction::DiscardSetting(key_for_discard.clone())),
                    "Discard"
                }
                span { style: "color:var(--muted);", "Expected type: {setting_value_kind_label(row.value_kind)}" }
            }
            if let Some(error) = row.validation_error {
                div { style: "color:#ff9b99;", "{error}" }
            }
        }
    }
}

#[component]
fn AdvancedView(model: Signal<AppModel>) -> Element {
    let state = {
        let app = model.read();
        app.state().clone_for_settings()
    };
    let advanced_rows = settings_for_domain(&state, SettingDomain::Advanced);
    let advanced_key_options = advanced_rows
        .iter()
        .map(|row| (row.key.clone(), row.title.clone()))
        .collect::<Vec<_>>();
    let has_preview = state.advanced_import_preview.is_some();

    rsx! {
        section {
            style: "display:flex; flex-direction:column; gap:12px; padding:16px; flex:1 1 auto; min-height:0; height:100%; overflow:hidden; box-sizing:border-box; position:relative; z-index:0;",
            div {
                style: "display:flex; flex-direction:column; gap:12px; min-height:0; flex:1 1 auto; overflow-y:auto; overflow-x:hidden; overscroll-behavior:contain; padding-right:2px; pointer-events:auto;",
                h2 { "Advanced Settings" }
                p { style: "color:var(--muted);", "about:config style controls for expert keys." }
                for row in advanced_rows.iter().cloned() {
                    {setting_editor_card(model, row)}
                }
                if advanced_rows.is_empty() {
                    {unsupported_card(
                        "No advanced keys surfaced",
                        "Advanced domain exists in DESIGN.md but no keys are currently listed in settings schema.",
                    )}
                }

                div {
                    style: "border:1px solid var(--line); border-radius:12px; padding:12px; background:rgba(9,16,33,0.75);",
                    h3 { "Advanced change history" }
                    p { style: "color:var(--muted); margin:0;", "Tracked locally with timestamps to support rollback and support debugging." }
                    if state.advanced_change_history.is_empty() {
                        p { style: "color:var(--muted); margin:8px 0 0;", "No advanced changes recorded in this app session." }
                    } else {
                        for change in state.advanced_change_history.iter() {
                            article {
                                key: "{change.changed_at.unix_timestamp_nanos()}-{change.key}",
                                style: "margin-top:10px; padding:10px; border:1px solid var(--line); border-radius:10px; background:rgba(5,12,24,0.7);",
                                div {
                                    style: "display:flex; gap:8px; align-items:center;",
                                    strong { "{change.key}" }
                                    span { style: "margin-left:auto; color:var(--muted);", "{format_timestamp(change.changed_at)}" }
                                }
                                p { style: "margin:8px 0 2px; color:var(--muted);", "Previous value" }
                                pre {
                                    style: "margin:0; white-space:pre-wrap; word-break:break-word;",
                                    "{change.previous_value_json}"
                                }
                                p { style: "margin:8px 0 2px; color:var(--muted);", "New value" }
                                pre {
                                    style: "margin:0; white-space:pre-wrap; word-break:break-word;",
                                    "{change.new_value_json}"
                                }
                            }
                        }
                    }
                }

                div {
                    style: "border:1px solid var(--line); border-radius:12px; padding:12px; background:rgba(9,16,33,0.75);",
                    h3 { "Advanced utilities" }
                    p { style: "color:var(--muted); margin:0;", "Reset advanced keys, export current advanced snapshot, and preview/apply advanced imports." }

                    div {
                        style: "display:flex; flex-wrap:wrap; gap:8px; align-items:center; margin-top:10px;",
                        select {
                            value: "{state.advanced_reset_key}",
                            onchange: move |event| model
                                .write()
                                .dispatch(AppAction::AdvancedResetKeySelected(event.value())),
                            if advanced_key_options.is_empty() {
                                option { value: "", "No advanced keys" }
                            } else {
                                for (key, title) in advanced_key_options.iter() {
                                    option { value: "{key}", "{title} ({key})" }
                                }
                            }
                        }
                        button {
                            r#type: "button",
                            disabled: advanced_key_options.is_empty(),
                            onclick: move |_| model.write().dispatch(AppAction::ResetSelectedAdvancedKey),
                            "Reset selected key"
                        }
                        button {
                            r#type: "button",
                            disabled: advanced_key_options.is_empty(),
                            onclick: move |_| model.write().dispatch(AppAction::ResetAllAdvancedSettings),
                            "Reset all advanced keys"
                        }
                    }

                    div {
                        style: "display:flex; gap:8px; align-items:center; margin-top:12px;",
                        button {
                            r#type: "button",
                            disabled: advanced_key_options.is_empty(),
                            onclick: move |_| model.write().dispatch(AppAction::ExportAdvancedSnapshot),
                            "Export advanced snapshot"
                        }
                        span { style: "color:var(--muted);", "JSON payload can be copied and reused for preview/apply import." }
                    }
                    textarea {
                        style: "margin-top:8px; min-height:120px;",
                        readonly: true,
                        value: "{state.advanced_export_snapshot_json}",
                    }

                    p { style: "color:var(--muted); margin:12px 0 4px;", "Import snapshot JSON" }
                    textarea {
                        style: "min-height:140px;",
                        value: "{state.advanced_import_snapshot_draft}",
                        oninput: move |event| model
                            .write()
                            .dispatch(AppAction::AdvancedImportSnapshotDraftChanged(event.value())),
                    }
                    div {
                        style: "display:flex; gap:8px; align-items:center; margin-top:8px;",
                        button {
                            r#type: "button",
                            onclick: move |_| model.write().dispatch(AppAction::PreviewAdvancedImportSnapshot),
                            "Preview import"
                        }
                        button {
                            r#type: "button",
                            disabled: !has_preview,
                            onclick: move |_| model.write().dispatch(AppAction::ApplyAdvancedImportSnapshot),
                            "Apply import"
                        }
                        span { style: "color:var(--muted);", "Apply is enabled after a successful preview." }
                    }

                    if let Some(preview) = &state.advanced_import_preview {
                        div {
                            style: "margin-top:10px; border:1px dashed #6a7fa6; border-radius:10px; padding:10px; background:rgba(5,12,24,0.7);",
                            h4 { style: "margin:0 0 8px;", "Import preview" }
                            p {
                                style: "margin:0; color:var(--muted);",
                                "Changes: {preview.changes.len()} • Unknown keys skipped: {preview.skipped_unknown.len()} • Non-advanced keys ignored: {preview.skipped_non_advanced.len()}"
                            }
                            if preview.changes.is_empty() {
                                p { style: "margin:8px 0 0; color:var(--muted);", "No advanced value changes detected." }
                            } else {
                                for change in preview.changes.iter() {
                                    article {
                                        key: "import-preview-{change.key}",
                                        style: "margin-top:10px; padding:10px; border:1px solid var(--line); border-radius:10px; background:rgba(7,15,29,0.75);",
                                        div {
                                            style: "display:flex; gap:8px; align-items:center;",
                                            strong { "{change.key}" }
                                            span { style: "margin-left:auto; color:var(--muted);", "Was: {change.source_before}" }
                                        }
                                        p { style: "margin:8px 0 2px; color:var(--muted);", "Previous value" }
                                        pre { style: "margin:0; white-space:pre-wrap; word-break:break-word;", "{change.previous_value_json}" }
                                        p { style: "margin:8px 0 2px; color:var(--muted);", "New value" }
                                        pre { style: "margin:0; white-space:pre-wrap; word-break:break-word;", "{change.new_value_json}" }
                                    }
                                }
                            }
                            if !preview.skipped_unknown.is_empty() {
                                p { style: "margin:10px 0 0; color:var(--muted);", "Unknown keys skipped: {preview.skipped_unknown.join(\", \")}" }
                            }
                            if !preview.skipped_non_advanced.is_empty() {
                                p { style: "margin:6px 0 0; color:var(--muted);", "Non-advanced keys ignored: {preview.skipped_non_advanced.join(\", \")}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
struct SettingRow {
    key: String,
    title: String,
    description: String,
    domain: SettingDomain,
    source: SettingSource,
    value_kind: SettingValueKind,
    draft_value: String,
    dirty: bool,
    validation_error: Option<String>,
}

fn settings_for_domain(state: &SettingsSnapshot, domain: SettingDomain) -> Vec<SettingRow> {
    state
        .entries
        .iter()
        .filter(|entry| entry.domain == domain)
        .map(|entry| {
            let editor = state.editors.get(&entry.key);
            SettingRow {
                key: entry.key.clone(),
                title: entry.title.clone(),
                description: entry.description.clone(),
                domain: entry.domain,
                source: entry.source.clone(),
                value_kind: entry.value_kind,
                draft_value: editor
                    .map(|editor| editor.draft_value.clone())
                    .unwrap_or_else(|| entry.value_json.clone()),
                dirty: editor.map(|editor| editor.dirty).unwrap_or(false),
                validation_error: editor.and_then(|editor| editor.validation_error.clone()),
            }
        })
        .collect()
}

fn map_shortcut(event: &KeyboardEvent) -> Option<ShortcutAction> {
    let key = event.key().to_string().to_lowercase();
    let code = event.code().to_string().to_lowercase();
    let modifiers = event.modifiers();
    let command = modifiers.contains(Modifiers::CONTROL) || modifiers.contains(Modifiers::META);
    let shift = modifiers.contains(Modifiers::SHIFT);

    if command && key == "q" {
        return Some(ShortcutAction::Quit);
    }
    if command && (key == "," || key == "comma" || code == "comma") {
        return Some(ShortcutAction::OpenSettings);
    }
    if command && shift && key == "s" {
        return Some(ShortcutAction::SkipCurrent);
    }
    if command && key == "arrowup" {
        return Some(ShortcutAction::VolumeUp);
    }
    if command && key == "arrowdown" {
        return Some(ShortcutAction::VolumeDown);
    }
    if shift && command && key == "/" {
        return Some(ShortcutAction::ToggleHelpMode);
    }
    if key == "escape" {
        return Some(ShortcutAction::ExitHelpMode);
    }
    if key == " " || key == "space" {
        return Some(ShortcutAction::TogglePlayback);
    }

    None
}

fn setting_value_kind_label(kind: SettingValueKind) -> String {
    match kind {
        SettingValueKind::Boolean => "boolean".to_string(),
        SettingValueKind::Integer => "integer".to_string(),
        SettingValueKind::Number => "number".to_string(),
        SettingValueKind::String => "string".to_string(),
        SettingValueKind::Object => "object".to_string(),
        SettingValueKind::Array => "array".to_string(),
        SettingValueKind::Enum { variants } => format!("enum ({})", variants.join(", ")),
    }
}

fn tab_button(mut model: Signal<AppModel>, active: Tab, tab: Tab, label: &'static str) -> Element {
    let active_style = if active == tab {
        "background:var(--accent); color:#04131f;"
    } else {
        "background:rgba(20,34,59,0.8); color:var(--text);"
    };

    rsx! {
        button {
            style: format!("{} border-radius:8px; border:1px solid var(--line); padding:6px 10px;", active_style),
            onclick: move |_| model.write().dispatch(AppAction::TabSelected(tab)),
            "{label}"
        }
    }
}

fn unsupported_card(title: &'static str, detail: &'static str) -> Element {
    rsx! {
        div {
            style: "padding:10px; margin-top:10px; border:1px dashed #6a7fa6; border-radius:10px; background:rgba(11,23,43,0.75);",
            strong { "{title}" }
            p { style: "margin-top:6px; color:var(--muted);", "{detail}" }
        }
    }
}

fn overlay_style() -> &'static str {
    "position:fixed; inset:0; background:rgba(4,8,15,0.65); display:flex; align-items:center; justify-content:center;"
}

fn modal_style() -> &'static str {
    "display:flex; flex-direction:column; gap:10px; min-width:420px; max-width:640px; background:#081526; border:1px solid var(--line); border-radius:12px; padding:14px;"
}

fn shell_style() -> &'static str {
    "
    --bg0:#060d19;
    --bg1:#0b1830;
    --line:#27466f;
    --text:#dbe6f5;
    --muted:#89a3c5;
    --accent:#48c3a7;
    color:var(--text);
    background:
        radial-gradient(circle at 12% 18%, rgba(52,104,171,0.35), transparent 42%),
        radial-gradient(circle at 84% 20%, rgba(49,166,149,0.2), transparent 36%),
        linear-gradient(160deg, var(--bg1), var(--bg0));
    position:fixed;
    inset:0;
    width:100vw;
    height:100vh;
    max-height:100vh;
    min-height:0;
    display:flex;
    flex-direction:column;
    overflow:hidden;
    font-family:'Iowan Old Style', 'Palatino', serif;
    "
}

#[derive(Clone)]
struct ChatLogsSnapshot {
    input_value: String,
    messages: Vec<MessageEntry>,
    busy: bool,
    status_message: Option<String>,
    twitch_status: TwitchStatus,
    youtube_status: YouTubeStatus,
    provider_statuses: Vec<RuntimeTtsProviderStatus>,
    playback_enabled: bool,
    active_playing_id: Option<u64>,
    selected_message_id: Option<u64>,
    sticky_to_latest: bool,
    hidden_message_count: usize,
    show_stop_confirmation: bool,
    show_quit_confirmation: bool,
    help_mode_enabled: bool,
}

#[derive(Clone)]
struct ChattersSnapshot {
    chatters: Vec<ChatterEntry>,
    chatter_search_draft: String,
    chatter_role_filter: ChatterRoleFilter,
    chatter_override_filter: ChatterOverrideFilter,
    selected_chatter_id: Option<String>,
    chatter_voice_draft: String,
    chatter_display_name_draft: String,
    chatter_notes_draft: String,
    chatter_status_message: Option<String>,
    help_mode_enabled: bool,
}

#[derive(Clone)]
struct SettingsSnapshot {
    entries: Vec<super::state::SettingDisplay>,
    editors: std::collections::HashMap<String, super::state::SettingEditorState>,
    settings_error: Option<String>,
    settings_status_message: Option<String>,
    twitch_status: TwitchStatus,
    youtube_status: YouTubeStatus,
    twitch_channel_login_draft: String,
    youtube_live_chat_id_draft: String,
    youtube_api_key_draft: String,
    openai_api_key_draft: String,
    orphan_count: usize,
    advanced_change_history: Vec<super::state::AdvancedSettingChange>,
    advanced_reset_key: String,
    advanced_export_snapshot_json: String,
    advanced_import_snapshot_draft: String,
    advanced_import_preview: Option<super::state::AdvancedImportPreview>,
}

impl AppState {
    fn clone_for_chat_logs(&self) -> ChatLogsSnapshot {
        ChatLogsSnapshot {
            input_value: self.input_value.clone(),
            messages: self.messages.clone(),
            busy: self.busy,
            status_message: self.status_message.clone(),
            twitch_status: self.twitch_status.clone(),
            youtube_status: self.youtube_status.clone(),
            provider_statuses: self.provider_statuses.clone(),
            playback_enabled: self.playback_enabled,
            active_playing_id: self.active_playing_id,
            selected_message_id: self.selected_message_id,
            sticky_to_latest: self.sticky_to_latest,
            hidden_message_count: self.hidden_message_count,
            show_stop_confirmation: self.show_stop_confirmation,
            show_quit_confirmation: self.show_quit_confirmation,
            help_mode_enabled: self.help_mode_enabled,
        }
    }

    fn clone_for_chatters(&self) -> ChattersSnapshot {
        ChattersSnapshot {
            chatters: self.chatter_entries.clone(),
            chatter_search_draft: self.chatter_search_draft.clone(),
            chatter_role_filter: self.chatter_role_filter,
            chatter_override_filter: self.chatter_override_filter,
            selected_chatter_id: self.selected_chatter_id.clone(),
            chatter_voice_draft: self.chatter_voice_draft.clone(),
            chatter_display_name_draft: self.chatter_display_name_draft.clone(),
            chatter_notes_draft: self.chatter_notes_draft.clone(),
            chatter_status_message: self.chatter_status_message.clone(),
            help_mode_enabled: self.help_mode_enabled,
        }
    }

    fn clone_for_settings(&self) -> SettingsSnapshot {
        SettingsSnapshot {
            entries: self
                .settings_entries
                .iter()
                .map(|entry| super::state::SettingDisplay {
                    key: entry.key.clone(),
                    title: entry.title.clone(),
                    description: entry.description.clone(),
                    domain: entry.domain,
                    source: entry.source.clone(),
                    value_kind: entry.value_kind,
                    value_json: entry.value_json.clone(),
                })
                .collect(),
            editors: self
                .settings_editors
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        super::state::SettingEditorState {
                            draft_value: value.draft_value.clone(),
                            original_value: value.original_value.clone(),
                            dirty: value.dirty,
                            validation_error: value.validation_error.clone(),
                        },
                    )
                })
                .collect(),
            settings_error: self.settings_error.clone(),
            settings_status_message: self.settings_status_message.clone(),
            twitch_status: self.twitch_status.clone(),
            youtube_status: self.youtube_status.clone(),
            twitch_channel_login_draft: self.twitch_channel_login_draft.clone(),
            youtube_live_chat_id_draft: self.youtube_live_chat_id_draft.clone(),
            youtube_api_key_draft: self.youtube_api_key_draft.clone(),
            openai_api_key_draft: self.openai_api_key_draft.clone(),
            orphan_count: self.orphan_count,
            advanced_change_history: self.advanced_change_history.clone(),
            advanced_reset_key: self.advanced_reset_key.clone(),
            advanced_export_snapshot_json: self.advanced_export_snapshot_json.clone(),
            advanced_import_snapshot_draft: self.advanced_import_snapshot_draft.clone(),
            advanced_import_preview: self.advanced_import_preview.clone(),
        }
    }
}

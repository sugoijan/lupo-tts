// SPDX-License-Identifier: Apache-2.0

//! Component tree and the WebSocket task. Styling lives in `index.html`.

use std::collections::BTreeMap;

use dioxus::prelude::*;
use futures_util::{SinkExt, StreamExt};
use gloo_net::websocket::{Message as WsMessage, futures::WebSocket};
use lupo_ui_protocol::{
    ChatConnection, ChatView, ClientCommand, LoginState, MessageState, MessageView, PlaybackView,
    ProviderState, ProviderView, SecretKind, ServerEvent,
};

const RECENT_LIMIT: usize = 30;
/// Retained message cap: terminal messages beyond this are evicted (oldest
/// first) so a multi-hour stream does not grow the wasm heap without bound.
const RETAIN_LIMIT: usize = 400;

/// Every signal the WebSocket task writes and the tree reads.
#[derive(Clone, Copy, PartialEq)]
struct Ui {
    connected: Signal<bool>,
    playback: Signal<PlaybackView>,
    volume: Signal<f32>,
    messages: Signal<BTreeMap<u64, MessageView>>,
    chat: Signal<Option<ChatView>>,
    providers: Signal<Vec<ProviderView>>,
    /// Last probe outcome: (provider, ok, detail).
    probe: Signal<Option<(String, bool, String)>>,
    error: Signal<Option<String>>,
}

fn apply(ui: &mut Ui, event: ServerEvent) {
    match event {
        ServerEvent::Snapshot {
            playback,
            volume,
            messages,
            chat,
            providers,
        } => {
            ui.playback.set(playback);
            ui.volume.set(volume);
            ui.messages
                .set(messages.into_iter().map(|m| (m.id, m)).collect());
            ui.chat.set(Some(chat));
            ui.providers.set(providers);
        }
        ServerEvent::Message { message } => {
            let mut messages = ui.messages.write();
            messages.insert(message.id, message);
            while messages.len() > RETAIN_LIMIT {
                let Some(id) = messages
                    .iter()
                    .find(|(_, m)| !m.state.is_pending())
                    .map(|(id, _)| *id)
                else {
                    break;
                };
                messages.remove(&id);
            }
        }
        ServerEvent::Playback { playback } => ui.playback.set(playback),
        ServerEvent::Volume { volume } => ui.volume.set(volume),
        ServerEvent::Cleared => ui.messages.write().clear(),
        ServerEvent::Chat { chat } => ui.chat.set(Some(chat)),
        ServerEvent::Providers { providers } => ui.providers.set(providers),
        ServerEvent::Probe {
            provider,
            ok,
            detail,
        } => ui.probe.set(Some((provider, ok, detail))),
        ServerEvent::Error { message } => ui.error.set(Some(message)),
    }
}

fn ws_url() -> Option<String> {
    let location = web_sys::window()?.location();
    let host = location.host().ok()?;
    let search = location.search().ok()?;
    let token = search
        .trim_start_matches('?')
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))?
        .to_string();
    Some(format!("ws://{host}/ws?token={token}"))
}

async fn ws_task(mut commands: UnboundedReceiver<ClientCommand>, mut ui: Ui) {
    let Some(url) = ws_url() else {
        ui.error
            .set(Some("missing token in the page URL".to_string()));
        return;
    };
    // A command whose send failed; retried on the next connection instead of
    // being dropped on the floor.
    let mut unsent: Option<ClientCommand> = None;
    loop {
        // `WebSocket::open` succeeds whenever the URL parses; only a received
        // frame proves the server is actually there, so the "connected" pill
        // flips on first receipt, not here.
        let Ok(socket) = WebSocket::open(&url) else {
            gloo_timers::future::TimeoutFuture::new(2_000).await;
            continue;
        };
        let mut socket = socket.fuse();
        let mut announced = false;
        if let Some(command) = unsent.take()
            && let Ok(json) = serde_json::to_string(&command)
            && socket.send(WsMessage::Text(json)).await.is_err()
        {
            unsent = Some(command);
            gloo_timers::future::TimeoutFuture::new(2_000).await;
            continue;
        }
        loop {
            futures_util::select! {
                incoming = socket.next() => {
                    let Some(Ok(message)) = incoming else { break };
                    if !announced {
                        announced = true;
                        ui.connected.set(true);
                    }
                    let WsMessage::Text(text) = message else { continue };
                    match serde_json::from_str::<ServerEvent>(&text) {
                        Ok(event) => apply(&mut ui, event),
                        Err(err) => ui.error.set(Some(format!("bad event: {err}"))),
                    }
                }
                command = commands.next() => {
                    let Some(command) = command else { return };
                    let Ok(json) = serde_json::to_string(&command) else { continue };
                    if socket.send(WsMessage::Text(json)).await.is_err() {
                        unsent = Some(command);
                        break;
                    }
                }
            }
        }
        ui.connected.set(false);
        gloo_timers::future::TimeoutFuture::new(2_000).await;
    }
}

#[component]
pub fn App() -> Element {
    let mut ui = Ui {
        connected: use_signal(|| false),
        playback: use_signal(|| PlaybackView {
            enabled: true,
            active_message_id: None,
        }),
        volume: use_signal(|| 1.0),
        messages: use_signal(BTreeMap::new),
        chat: use_signal(|| None),
        providers: use_signal(Vec::new),
        probe: use_signal(|| None),
        error: use_signal(|| None),
    };
    let ws = use_coroutine(move |rx| ws_task(rx, ui));
    let mut show_settings = use_signal(|| false);

    let connected = *ui.connected.read();
    let playback = ui.playback.read().clone();
    let volume = *ui.volume.read();
    let error = ui.error.read().clone();

    rsx! {
        header {
            button {
                class: if playback.enabled { "" } else { "primary" },
                onclick: move |_| {
                    let enabled = ui.playback.read().enabled;
                    ws.send(if enabled { ClientCommand::Pause } else { ClientCommand::Resume });
                },
                if playback.enabled { "Pause" } else { "Resume" }
            }
            button { onclick: move |_| ws.send(ClientCommand::Stop), "Stop & pause" }
            button {
                class: "danger",
                onclick: move |_| ws.send(ClientCommand::Clear),
                "Clear queue"
            }
            label { class: "volume",
                "Vol"
                input {
                    r#type: "range",
                    min: "0",
                    max: "200",
                    value: "{(volume * 100.0) as i32}",
                    onchange: move |event| {
                        if let Ok(percent) = event.value().parse::<f32>() {
                            ws.send(ClientCommand::SetVolume { volume: percent / 100.0 });
                        }
                    },
                }
            }
            button {
                class: if *show_settings.read() { "primary gear" } else { "gear" },
                onclick: move |_| {
                    let current = *show_settings.read();
                    show_settings.set(!current);
                },
                "Settings"
            }
            div { class: "status",
                span {
                    class: if connected { "pill on" } else { "pill down" },
                    if connected { "connected" } else { "disconnected — retrying" }
                }
                span {
                    class: if playback.enabled { "pill on" } else { "pill off" },
                    if playback.enabled { "playback on" } else { "paused" }
                }
                ChatPill { ui }
            }
        }
        if let Some(message) = error {
            div { class: "toast",
                span { "{message}" }
                button { class: "small", onclick: move |_| ui.error.set(None), "×" }
            }
        }
        main {
            if *show_settings.read() {
                SettingsPanel { ui, ws }
            }
            MessageLists { ui, ws }
        }
    }
}

#[component]
fn ChatPill(ui: Ui) -> Element {
    let chat = ui.chat.read().clone();
    let (class, text) = match &chat {
        None => ("pill", "chat: …".to_string()),
        Some(chat) => match &chat.connection {
            ChatConnection::Connected { channel } => ("pill on", format!("reading #{channel}")),
            ChatConnection::Connecting => ("pill", "chat: connecting…".to_string()),
            ChatConnection::CredentialsMissing => ("pill off", "chat: not logged in".to_string()),
            ChatConnection::Idle => ("pill", "chat: idle".to_string()),
            ChatConnection::Disconnected => ("pill off", "chat: disconnected".to_string()),
            ChatConnection::Error { message } => ("pill down", format!("chat: {message}")),
        },
    };
    rsx! {
        span { class: "{class}", "{text}" }
    }
}

#[component]
fn SettingsPanel(ui: Ui, ws: Coroutine<ClientCommand>) -> Element {
    let chat = ui.chat.read().clone();
    let mut channel_draft = use_signal(String::new);
    let mut key_draft = use_signal(String::new);
    let mut message_draft = use_signal(|| "Hello from the LUPO dock.".to_string());
    let providers = ui.providers.read().clone();
    let probe = ui.probe.read().clone();

    rsx! {
        section { class: "settings",
            div { class: "group",
                h2 { "Twitch" }
                if let Some(chat) = &chat {
                    match (&chat.account, &chat.login) {
                        (_, LoginState::Starting) => rsx! {
                            p { "Contacting Twitch for a login code…" }
                        },
                        (_, LoginState::Pending { user_code, verification_uri }) => rsx! {
                            p {
                                "Enter code "
                                strong { class: "code", "{user_code}" }
                                " at "
                                a { href: "{verification_uri}", target: "_blank", "{verification_uri}" }
                                " — approve in the browser, this page updates by itself."
                            }
                        },
                        // Failed comes before the logged-in arm: a failed
                        // re-login must show its error (and a retry) even while
                        // an account is still linked.
                        (account, LoginState::Failed { message }) => rsx! {
                            p { class: "err", "Login failed: {message}" }
                            if let Some(account) = account {
                                p { "Logged in as " strong { "{account}" } }
                                button {
                                    onclick: move |_| ws.send(ClientCommand::TwitchLogout),
                                    "Forget Twitch login"
                                }
                            }
                            TwitchConnect { ws }
                        },
                        (Some(account), _) => rsx! {
                            p { "Logged in as " strong { "{account}" } }
                            // Revoked/expired credentials surface as a chat
                            // connection error; offer a re-login right here
                            // rather than only "forget".
                            if matches!(chat.connection, ChatConnection::Error { .. }) {
                                p { class: "err",
                                    "Twitch rejected the stored login (password changed or access revoked?). Re-connect the account:"
                                }
                                TwitchConnect { ws }
                            }
                            button {
                                onclick: move |_| ws.send(ClientCommand::TwitchLogout),
                                "Forget Twitch login"
                            }
                        },
                        (None, _) => rsx! {
                            TwitchConnect { ws }
                        },
                    }
                    div { class: "row",
                        input {
                            placeholder: chat.channel.clone().unwrap_or_else(|| "channel (blank = your own)".to_string()),
                            value: "{channel_draft}",
                            oninput: move |event| channel_draft.set(event.value()),
                        }
                        button {
                            onclick: move |_| ws.send(ClientCommand::SetChannel { channel: channel_draft.read().clone() }),
                            "Set channel"
                        }
                    }
                }
            }
            div { class: "group",
                h2 { "Voices" }
                ul { class: "providers",
                    for provider in providers {
                        li {
                            span { class: "provider-state {provider_state_class(provider.state)}" }
                            "{provider.label}"
                            if let Some(detail) = &provider.detail {
                                span { class: "dim", " — {detail}" }
                            }
                        }
                    }
                }
                div { class: "row",
                    input {
                        r#type: "password",
                        placeholder: "OpenAI API key (stored in LUPO's secret store)",
                        value: "{key_draft}",
                        oninput: move |event| key_draft.set(event.value()),
                    }
                    button {
                        onclick: move |_| {
                            let value = key_draft.read().clone();
                            ws.send(ClientCommand::SetSecret { secret: SecretKind::OpenAiApiKey, value });
                            key_draft.set(String::new());
                        },
                        "Save key"
                    }
                }
                div { class: "row",
                    button {
                        onclick: move |_| ws.send(ClientCommand::Probe { provider: "openai".to_string() }),
                        "Test OpenAI key"
                    }
                    button {
                        onclick: move |_| ws.send(ClientCommand::ForgetSecret { secret: SecretKind::OpenAiApiKey }),
                        "Forget key"
                    }
                }
                if let Some((provider, ok, detail)) = probe {
                    p { class: if ok { "ok" } else { "err" }, "{provider}: {detail}" }
                }
            }
            div { class: "group",
                h2 { "Test" }
                div { class: "row",
                    input {
                        value: "{message_draft}",
                        oninput: move |event| message_draft.set(event.value()),
                    }
                    button {
                        onclick: move |_| ws.send(ClientCommand::Submit { text: message_draft.read().clone() }),
                        "Queue message"
                    }
                }
                p { class: "dim",
                    "To hear it on your speakers, set the source's Audio Monitoring to \"Monitor and Output\" in Advanced Audio Properties."
                }
            }
        }
    }
}

#[component]
fn TwitchConnect(ws: Coroutine<ClientCommand>) -> Element {
    rsx! {
        button {
            class: "primary",
            onclick: move |_| ws.send(ClientCommand::TwitchLogin { open_browser: false }),
            "Connect Twitch account"
        }
        p { class: "dim", "Uses a device code shown here; nothing opens behind your back." }
    }
}

fn provider_state_class(state: ProviderState) -> &'static str {
    match state {
        ProviderState::Ready => "ok",
        ProviderState::Unavailable => "off",
        ProviderState::Error => "err",
    }
}

#[component]
fn MessageLists(ui: Ui, ws: Coroutine<ClientCommand>) -> Element {
    let playback = ui.playback.read().clone();
    // Clone only the cards actually rendered, not the whole retained map.
    let messages = ui.messages.read();
    let playing: Vec<MessageView> = messages
        .values()
        .filter(|m| m.state == MessageState::Playing || Some(m.id) == playback.active_message_id)
        .cloned()
        .collect();
    let queue: Vec<MessageView> = messages
        .values()
        .filter(|m| m.state.is_pending() && !playing.iter().any(|p| p.id == m.id))
        .cloned()
        .collect();
    let recent: Vec<MessageView> = messages
        .values()
        .filter(|m| !m.state.is_pending() && !playing.iter().any(|p| p.id == m.id))
        .rev()
        .take(RECENT_LIMIT)
        .cloned()
        .collect();
    drop(messages);

    rsx! {
        h2 { "Now playing" }
        if playing.is_empty() {
            div { class: "empty", "nothing" }
        }
        for message in playing {
            MessageCard { ui, ws, message, actions: CardActions::Playing }
        }
        h2 {
            "Queue "
            if !queue.is_empty() {
                span { class: "dim", "({queue.len()})" }
            }
        }
        if queue.is_empty() {
            div { class: "empty", "empty" }
        }
        for message in queue {
            MessageCard { ui, ws, message, actions: CardActions::Queued }
        }
        h2 { "Recent" }
        if recent.is_empty() {
            div { class: "empty", "nothing yet" }
        }
        for message in recent {
            MessageCard { ui, ws, message, actions: CardActions::Recent }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum CardActions {
    Playing,
    Queued,
    Recent,
}

#[component]
fn MessageCard(
    ui: Ui,
    ws: Coroutine<ClientCommand>,
    message: MessageView,
    actions: CardActions,
) -> Element {
    let id = message.id;
    let state = format!("{:?}", message.state).to_lowercase();
    let playing = ui.playback.read().active_message_id == Some(id);
    rsx! {
        div {
            class: if playing { "msg playing" } else { "msg" },
            div { class: "who",
                "{message.sender}"
                span { class: "state", "{state} · {message.provider}" }
            }
            div { class: "actions",
                match actions {
                    CardActions::Playing => rsx! {
                        button { class: "small", onclick: move |_| ws.send(ClientCommand::Skip { id }), "Skip" }
                    },
                    CardActions::Queued => rsx! {
                        button { class: "small", onclick: move |_| ws.send(ClientCommand::PlayNext { id }), "Next" }
                        button { class: "small", onclick: move |_| ws.send(ClientCommand::Skip { id }), "Skip" }
                    },
                    CardActions::Recent => rsx! {
                        button { class: "small", onclick: move |_| ws.send(ClientCommand::Replay { id }), "Replay" }
                    },
                }
            }
            div { class: "text", "{message.content}" }
        }
    }
}

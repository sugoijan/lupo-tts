// SPDX-License-Identifier: Apache-2.0

//! Wire protocol between the LUPO runtime and its thin web UIs (the OBS dock
//! today; any webview shell tomorrow). JSON over a local WebSocket, tagged by
//! `type`; the server sends a full [`ServerEvent::Snapshot`] on connect and
//! deltas afterwards.
//!
//! This crate is deliberately dependency-light (serde only) so the same types
//! compile natively on the server side and to wasm inside the UI. Conversions
//! from runtime/persistence types live in `lupo-runtime`, not here.

use serde::{Deserialize, Serialize};

/// Lifecycle states, mirroring `lupo_persistence::MessageStateKind` (the
/// conversion lives in `lupo-runtime` so this crate stays wasm-clean).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageState {
    Arrived,
    Filtered,
    Transforming,
    Transformed,
    Synthesizing,
    Synthesized,
    Queued,
    Playing,
    Played,
    Skipped,
    Error,
}

impl MessageState {
    /// Not yet played, skipped or failed: shown in the queue section.
    pub fn is_pending(self) -> bool {
        !matches!(
            self,
            MessageState::Playing
                | MessageState::Played
                | MessageState::Skipped
                | MessageState::Error
        )
    }
}

/// Everything the operator view needs about one message.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageView {
    pub id: u64,
    pub provider: String,
    pub sender: String,
    pub content: String,
    pub state: MessageState,
    /// Unix time in milliseconds.
    pub received_at_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PlaybackView {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub active_message_id: Option<u64>,
}

/// Chat connection state, independent of any in-progress login.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ChatConnection {
    Idle,
    Connecting,
    Connected { channel: String },
    CredentialsMissing,
    Disconnected,
    Error { message: String },
}

/// Device-code login progress.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LoginState {
    Idle,
    Starting,
    Pending {
        user_code: String,
        verification_uri: String,
    },
    Failed {
        message: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatView {
    /// Linked Twitch account login, if any.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub account: Option<String>,
    /// Channel override (`None` = the linked account's own channel).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub channel: Option<String>,
    pub connection: ChatConnection,
    pub login: LoginState,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderState {
    Ready,
    Unavailable,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderView {
    pub id: String,
    pub label: String,
    pub state: ProviderState,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub detail: Option<String>,
}

/// Secrets the UI may set or forget; the canonical store keys stay server-side.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    OpenAiApiKey,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    /// Full state, sent once per connection (and after an event-stream overrun).
    Snapshot {
        playback: PlaybackView,
        /// Pipeline output gain, 0.0..=2.0.
        volume: f32,
        messages: Vec<MessageView>,
        chat: ChatView,
        providers: Vec<ProviderView>,
    },
    /// One message changed (or appeared).
    Message {
        message: MessageView,
    },
    Playback {
        playback: PlaybackView,
    },
    Volume {
        volume: f32,
    },
    /// The queue was cleared; drop every message.
    Cleared,
    Chat {
        chat: ChatView,
    },
    Providers {
        providers: Vec<ProviderView>,
    },
    /// Outcome of a [`ClientCommand::Probe`].
    Probe {
        provider: String,
        ok: bool,
        detail: String,
    },
    /// A command failed.
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientCommand {
    Pause,
    Resume,
    /// Stop the current message and pause.
    Stop,
    Clear,
    Skip {
        id: u64,
    },
    /// Re-queue a played, skipped or failed message.
    Replay {
        id: u64,
    },
    /// Move a queued message to the front.
    PlayNext {
        id: u64,
    },
    SetVolume {
        volume: f32,
    },
    /// Queue a manual message through the full pipeline and nudge playback.
    Submit {
        text: String,
    },
    TwitchLogin {
        open_browser: bool,
    },
    TwitchLogout,
    /// Set the channel override; an empty string clears it.
    SetChannel {
        channel: String,
    },
    SetSecret {
        secret: SecretKind,
        value: String,
    },
    ForgetSecret {
        secret: SecretKind,
    },
    /// Synthesise a short phrase with one provider; nothing is played.
    Probe {
        provider: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_round_trip_as_tagged_json() {
        let json = serde_json::to_string(&ClientCommand::Skip { id: 7 }).unwrap();
        assert_eq!(json, r#"{"type":"skip","id":7}"#);
        let parsed: ClientCommand = serde_json::from_str(r#"{"type":"pause"}"#).unwrap();
        assert_eq!(parsed, ClientCommand::Pause);
        let json = serde_json::to_string(&ClientCommand::SetSecret {
            secret: SecretKind::OpenAiApiKey,
            value: "k".into(),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"set_secret","secret":"open_ai_api_key","value":"k"}"#
        );
    }

    #[test]
    fn events_use_snake_case_tags() {
        let json = serde_json::to_string(&ServerEvent::Chat {
            chat: ChatView {
                account: None,
                channel: None,
                connection: ChatConnection::CredentialsMissing,
                login: LoginState::Idle,
            },
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"chat","chat":{"connection":{"state":"credentials_missing"},"login":{"state":"idle"}}}"#
        );
    }

    #[test]
    fn pending_states_split_queue_from_history() {
        assert!(MessageState::Queued.is_pending());
        assert!(MessageState::Synthesizing.is_pending());
        assert!(!MessageState::Playing.is_pending());
        assert!(!MessageState::Error.is_pending());
    }
}

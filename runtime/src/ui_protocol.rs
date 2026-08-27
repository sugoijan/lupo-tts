// SPDX-License-Identifier: Apache-2.0

//! Re-export of the wasm-safe wire protocol plus the conversions from runtime
//! and persistence types (kept here so `lupo-ui-protocol` compiles to wasm).

pub use lupo_ui_protocol::*;

use lupo_persistence::{MessageSnapshot, MessageStateKind};

use crate::pipeline::{PipelinePlaybackState, RuntimeTtsProviderState, RuntimeTtsProviderStatus};
use crate::twitch::TwitchStatus;

pub fn message_state(kind: MessageStateKind) -> MessageState {
    match kind {
        MessageStateKind::Arrived => MessageState::Arrived,
        MessageStateKind::Filtered => MessageState::Filtered,
        MessageStateKind::Transforming => MessageState::Transforming,
        MessageStateKind::Transformed => MessageState::Transformed,
        MessageStateKind::Synthesizing => MessageState::Synthesizing,
        MessageStateKind::Synthesized => MessageState::Synthesized,
        MessageStateKind::Queued => MessageState::Queued,
        MessageStateKind::Playing => MessageState::Playing,
        MessageStateKind::Played => MessageState::Played,
        MessageStateKind::Skipped => MessageState::Skipped,
        MessageStateKind::Error => MessageState::Error,
    }
}

pub fn message_view(snapshot: &MessageSnapshot) -> MessageView {
    MessageView {
        id: snapshot.header.id,
        provider: snapshot.header.provider.clone(),
        sender: snapshot.header.sender.clone(),
        content: snapshot.header.content.clone(),
        state: message_state(snapshot.state_kind()),
        received_at_ms: (snapshot.header.received_at.unix_timestamp_nanos() / 1_000_000) as i64,
    }
}

pub fn playback_view(state: &PipelinePlaybackState) -> PlaybackView {
    PlaybackView {
        enabled: state.enabled,
        active_message_id: state.active_message_id,
    }
}

pub fn provider_view(status: &RuntimeTtsProviderStatus) -> ProviderView {
    ProviderView {
        id: status.provider_id.clone(),
        label: status.label.clone(),
        state: match status.state {
            RuntimeTtsProviderState::Ready => ProviderState::Ready,
            RuntimeTtsProviderState::Unavailable => ProviderState::Unavailable,
            RuntimeTtsProviderState::Error => ProviderState::Error,
        },
        detail: status.detail.clone(),
    }
}

pub fn chat_connection(status: &TwitchStatus) -> ChatConnection {
    match status {
        TwitchStatus::Idle => ChatConnection::Idle,
        TwitchStatus::Connecting => ChatConnection::Connecting,
        TwitchStatus::Connected { channel } => ChatConnection::Connected {
            channel: channel.clone(),
        },
        TwitchStatus::CredentialsMissing => ChatConnection::CredentialsMissing,
        TwitchStatus::Error(message) => ChatConnection::Error {
            message: message.clone(),
        },
        TwitchStatus::Disconnected => ChatConnection::Disconnected,
    }
}

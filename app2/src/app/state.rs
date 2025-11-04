// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use iced::window;
use lupo_configuration::{SettingDomain, SettingRecord, SettingSource, SettingValueKind};
use lupo_persistence::{MessageSnapshot, MessageState, SkipReason, SkippedState};
use serde_json::to_string_pretty;
use time::OffsetDateTime;

use crate::runtime::twitch::TwitchStatus;

pub const HISTORY_LIMIT: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    ChatLogs,
    Chatters,
    Settings,
    Advanced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageProvider {
    Manual,
    Twitch,
    Unknown,
}

impl MessageProvider {
    pub fn from_str(value: &str) -> Self {
        match value {
            "manual" => MessageProvider::Manual,
            "twitch" | "twitch_chat" => MessageProvider::Twitch,
            _ => MessageProvider::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MessageProvider::Manual => "Manual",
            MessageProvider::Twitch => "Twitch",
            MessageProvider::Unknown => "Message",
        }
    }
}

#[derive(Clone, Debug)]
pub enum MessageStatus {
    Arrived,
    Transforming,
    Transformed,
    Synthesizing,
    Synthesized,
    Queued,
    Playing,
    Played,
    Skipped(String),
    Error(String),
}

impl MessageStatus {
    pub fn from_state(state: &MessageState) -> Self {
        match state {
            MessageState::Arrived | MessageState::Filtered => MessageStatus::Arrived,
            MessageState::Transforming(_) => MessageStatus::Transforming,
            MessageState::Transformed(_) => MessageStatus::Transformed,
            MessageState::Synthesizing(_) => MessageStatus::Synthesizing,
            MessageState::Synthesized(_) => MessageStatus::Synthesized,
            MessageState::Queued(_) => MessageStatus::Queued,
            MessageState::Playing(_) => MessageStatus::Playing,
            MessageState::Played(_) => MessageStatus::Played,
            MessageState::Skipped(skipped) => MessageStatus::Skipped(skip_reason_label(skipped)),
            MessageState::Error(error) => {
                let detail = error
                    .detail
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|| error.code.clone());
                MessageStatus::Error(detail)
            }
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            MessageStatus::Arrived => "Arrived",
            MessageStatus::Transforming => "Transforming",
            MessageStatus::Transformed => "Transformed",
            MessageStatus::Synthesizing => "Synthesizing",
            MessageStatus::Synthesized => "Synthesized",
            MessageStatus::Queued => "Queued",
            MessageStatus::Playing => "Playing",
            MessageStatus::Played => "Played",
            MessageStatus::Skipped(_) => "Skipped",
            MessageStatus::Error(_) => "Error",
        }
    }
}

#[derive(Clone)]
pub struct MessageEntry {
    pub id: u64,
    pub sender: String,
    pub content: String,
    pub display_text: String,
    pub provider: MessageProvider,
    pub status: MessageStatus,
    pub received_at: OffsetDateTime,
}

impl MessageEntry {
    pub fn from_snapshot(snapshot: MessageSnapshot, display_text: String) -> Self {
        let id = snapshot.header.id;
        let provider = MessageProvider::from_str(&snapshot.header.provider);

        Self {
            id,
            sender: snapshot.header.sender.clone(),
            content: snapshot.header.content.clone(),
            display_text,
            provider,
            status: MessageStatus::from_state(&snapshot.lifecycle.state),
            received_at: snapshot.header.received_at,
        }
    }

    pub fn apply_snapshot(&mut self, snapshot: MessageSnapshot, display_text: String) {
        self.sender = snapshot.header.sender.clone();
        self.content = snapshot.header.content.clone();
        self.display_text = display_text;
        self.provider = MessageProvider::from_str(&snapshot.header.provider);
        self.status = MessageStatus::from_state(&snapshot.lifecycle.state);
        self.received_at = snapshot.header.received_at;
    }

    pub fn status_badge(&self) -> String {
        match &self.status {
            MessageStatus::Error(msg) | MessageStatus::Skipped(msg) => msg.clone(),
            other => other.label().to_string(),
        }
    }
}

pub struct SettingDisplay {
    pub key: String,
    pub title: String,
    pub description: String,
    pub domain: SettingDomain,
    pub source: SettingSource,
    pub value_kind: SettingValueKind,
    pub value_json: String,
}

impl From<SettingRecord> for SettingDisplay {
    fn from(record: SettingRecord) -> Self {
        let value_json =
            to_string_pretty(&record.value).unwrap_or_else(|_| "<invalid json>".into());
        Self {
            key: record.key,
            title: record.title,
            description: record.description,
            domain: record.domain,
            source: record.source,
            value_kind: record.value_kind,
            value_json,
        }
    }
}

pub struct SettingEditorState {
    pub draft_value: String,
    pub original_value: String,
    pub dirty: bool,
    pub validation_error: Option<String>,
}

pub struct AppState {
    pub input_value: String,
    pub messages: Vec<MessageEntry>,
    pub message_index: HashMap<u64, usize>,
    pub busy: bool,
    pub status_message: Option<String>,
    pub twitch_status: TwitchStatus,
    pub settings_entries: Vec<SettingDisplay>,
    pub settings_editors: HashMap<String, SettingEditorState>,
    pub settings_error: Option<String>,
    pub settings_status_message: Option<String>,
    pub orphan_count: usize,
    pub active_tab: Tab,
    pub show_advanced: bool,
    pub history_limit: usize,
    pub playback_enabled: bool,
    pub active_playing_id: Option<u64>,
    pub sticky_to_latest: bool,
    pub hidden_message_count: usize,
    pub show_stop_confirmation: bool,
    pub show_quit_confirmation: bool,
    pub quit_after_stop: bool,
    pub pending_quit_window_id: Option<window::Id>,
    pub message_scroll_offset: f32,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            input_value: String::new(),
            messages: Vec::new(),
            message_index: HashMap::new(),
            busy: false,
            status_message: None,
            twitch_status: TwitchStatus::Idle,
            settings_entries: Vec::new(),
            settings_editors: HashMap::new(),
            settings_error: None,
            settings_status_message: None,
            orphan_count: 0,
            active_tab: Tab::ChatLogs,
            show_advanced: false,
            history_limit: HISTORY_LIMIT,
            playback_enabled: true,
            active_playing_id: None,
            sticky_to_latest: true,
            hidden_message_count: 0,
            show_stop_confirmation: false,
            show_quit_confirmation: false,
            quit_after_stop: false,
            pending_quit_window_id: None,
            message_scroll_offset: 1.0,
        }
    }
}

impl AppState {
    pub fn rebuild_index(&mut self) {
        self.message_index.clear();
        for (idx, entry) in self.messages.iter().enumerate() {
            self.message_index.insert(entry.id, idx);
        }
    }

    pub fn trim_history(&mut self) {
        if self.messages.len() > self.history_limit {
            self.messages.truncate(self.history_limit);
            self.rebuild_index();
        }
    }

    pub fn is_at_latest(&self) -> bool {
        self.message_scroll_offset >= 0.98
    }

    pub fn update_sticky_state_from_scroll(&mut self) {
        if self.is_at_latest() {
            self.sticky_to_latest = true;
            self.hidden_message_count = 0;
        } else {
            self.sticky_to_latest = false;
        }
    }

    pub fn has_pending_playback(&self) -> bool {
        self.messages.iter().any(|entry| {
            matches!(
                entry.status,
                MessageStatus::Arrived
                    | MessageStatus::Transforming
                    | MessageStatus::Transformed
                    | MessageStatus::Synthesizing
                    | MessageStatus::Synthesized
                    | MessageStatus::Queued
                    | MessageStatus::Playing
            )
        })
    }

    pub fn reduce_stop_continue(&mut self) {
        self.show_stop_confirmation = false;
        self.quit_after_stop = false;
        self.pending_quit_window_id = None;
        self.status_message = Some("Playback continues.".to_string());
    }

    pub fn reduce_stop_cancel(&mut self) {
        self.show_stop_confirmation = false;
        self.quit_after_stop = false;
        self.pending_quit_window_id = None;
        self.status_message = Some("Stop cancelled.".to_string());
    }

    pub fn reduce_quit_stop_before(&mut self) {
        self.show_quit_confirmation = false;
        self.show_stop_confirmation = true;
        self.quit_after_stop = true;
        self.status_message = Some("Confirm stop before quitting.".to_string());
    }

    pub fn reduce_quit_cancel(&mut self) {
        self.show_quit_confirmation = false;
        self.quit_after_stop = false;
        self.pending_quit_window_id = None;
        self.status_message = Some("Quit cancelled.".to_string());
    }

    pub fn reduce_quit_requested(&mut self, window_id: Option<window::Id>) -> bool {
        if self.has_pending_playback() {
            self.show_quit_confirmation = true;
            self.show_stop_confirmation = false;
            self.quit_after_stop = false;
            self.pending_quit_window_id = window_id;
            self.status_message = Some("Quit requested while playback is active.".to_string());
            true
        } else {
            self.pending_quit_window_id = window_id;
            false
        }
    }
}

fn skip_reason_label(skipped: &SkippedState) -> String {
    let base = match &skipped.reason {
        SkipReason::OperatorOverride => "Operator override",
        SkipReason::FilterRule => "Filtered",
        SkipReason::CommandFiltered => "Command filtered",
        SkipReason::CommandRejected => "Command rejected",
        SkipReason::ProviderBotSuppressed => "Bot suppressed",
        SkipReason::PaidMessageSuppressed => "Paid message filtered",
        SkipReason::PredictedQueueOverflow => "Predicted overflow",
        SkipReason::QueueOverflow => "Queue overflow",
        SkipReason::ProviderRejected => "Provider rejected",
        SkipReason::ModeratorDeleted => "Moderator deleted",
        SkipReason::Other(message) => message.as_str(),
    };

    if let Some(note) = &skipped.note {
        format!("{base}: {note}")
    } else {
        base.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn queued_entry() -> MessageEntry {
        MessageEntry {
            id: 1,
            sender: "alice".to_string(),
            content: "hello".to_string(),
            display_text: "hello".to_string(),
            provider: MessageProvider::Manual,
            status: MessageStatus::Queued,
            received_at: OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn reduce_quit_requested_shows_confirmation_when_playback_pending() {
        let mut state = AppState::default();
        state.messages.push(queued_entry());

        let should_intercept = state.reduce_quit_requested(None);
        assert!(should_intercept);
        assert!(state.show_quit_confirmation);
        assert!(!state.quit_after_stop);
    }

    #[test]
    fn reduce_stop_continue_clears_stop_prompt_state() {
        let mut state = AppState {
            show_stop_confirmation: true,
            quit_after_stop: true,
            pending_quit_window_id: Some(window::Id::unique()),
            ..AppState::default()
        };

        state.reduce_stop_continue();
        assert!(!state.show_stop_confirmation);
        assert!(!state.quit_after_stop);
        assert!(state.pending_quit_window_id.is_none());
    }

    #[test]
    fn reduce_quit_stop_before_sets_stop_confirmation() {
        let mut state = AppState {
            show_quit_confirmation: true,
            ..AppState::default()
        };

        state.reduce_quit_stop_before();
        assert!(!state.show_quit_confirmation);
        assert!(state.show_stop_confirmation);
        assert!(state.quit_after_stop);
    }
}

// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use lupo_configuration::{SettingDomain, SettingRecord, SettingSource, SettingValueKind};
use lupo_persistence::{
    Artifact, MessageSnapshot, MessageState, SkipReason, SkippedState, SynthBackendId,
    canonical_chatter_id,
};
use serde_json::Value;
use serde_json::to_string_pretty;
use time::OffsetDateTime;
use url::Url;

use crate::runtime::pipeline::RuntimeTtsProviderStatus;
use crate::runtime::twitch::TwitchStatus;
use crate::runtime::youtube::YouTubeStatus;

pub const HISTORY_LIMIT: usize = 500;
pub const ADVANCED_CHANGE_HISTORY_LIMIT: usize = 100;
const EMOTE_ASSET_ROUTE_PREFIX: &str = "/emote/";

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
    YouTube,
    Unknown,
}

impl MessageProvider {
    pub fn from_str(value: &str) -> Self {
        match value {
            "manual" | "local" => MessageProvider::Manual,
            "twitch" | "twitch_chat" => MessageProvider::Twitch,
            "youtube_live_chat" => MessageProvider::YouTube,
            _ => MessageProvider::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MessageProvider::Manual => "Local",
            MessageProvider::Twitch => "Twitch",
            MessageProvider::YouTube => "YouTube",
            MessageProvider::Unknown => "Message",
        }
    }
}

#[derive(Clone)]
pub enum MessageDisplaySegment {
    Text(String),
    Emote { code: String, url: Option<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatterRoleFilter {
    All,
    Moderators,
    ChannelOwner,
}

impl ChatterRoleFilter {
    pub fn from_ui(value: &str) -> Self {
        match value {
            "moderators" => ChatterRoleFilter::Moderators,
            "channel_owner" => ChatterRoleFilter::ChannelOwner,
            _ => ChatterRoleFilter::All,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatterOverrideFilter {
    All,
    HasVoiceOverride,
    NoVoiceOverride,
}

impl ChatterOverrideFilter {
    pub fn from_ui(value: &str) -> Self {
        match value {
            "has_voice_override" => ChatterOverrideFilter::HasVoiceOverride,
            "no_voice_override" => ChatterOverrideFilter::NoVoiceOverride,
            _ => ChatterOverrideFilter::All,
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
    pub chatter_id: String,
    pub provider_id: String,
    pub sender: String,
    pub user_id: Option<String>,
    pub chatter_login: Option<String>,
    pub badges: Vec<String>,
    pub sender_color: Option<String>,
    pub content: String,
    pub display_text: String,
    pub display_segments: Vec<MessageDisplaySegment>,
    pub provider: MessageProvider,
    pub status: MessageStatus,
    pub synthesis_trace: Option<String>,
    pub source_timestamp: Option<OffsetDateTime>,
    pub received_at: OffsetDateTime,
    pub profile_link: Option<String>,
}

impl MessageEntry {
    pub fn from_snapshot(snapshot: MessageSnapshot, display_text: String) -> Self {
        let id = snapshot.header.id;
        let provider_id = snapshot.header.provider.clone();
        let provider = MessageProvider::from_str(&snapshot.header.provider);
        let chatter_id = canonical_chatter_id(
            &snapshot.header.provider,
            snapshot.header.user_id.as_deref(),
            &snapshot.header.sender,
        );
        let user_id = snapshot.header.user_id.clone();
        let chatter_login = metadata_string(&snapshot, "chatter_login");
        let badges = metadata_string_array(&snapshot, "badges");
        let sender_color = metadata_string(&snapshot, "color");
        let display_segments = display_segments_from_snapshot(&snapshot);
        let profile_link = profile_link_for_snapshot(&snapshot, chatter_login.as_deref());

        Self {
            id,
            chatter_id,
            provider_id,
            sender: snapshot.header.sender.clone(),
            user_id,
            chatter_login,
            badges,
            sender_color,
            content: snapshot.header.content.clone(),
            display_text,
            display_segments,
            provider,
            status: MessageStatus::from_state(&snapshot.lifecycle.state),
            synthesis_trace: synthesis_trace(&snapshot),
            source_timestamp: snapshot.header.source_timestamp,
            received_at: snapshot.header.received_at,
            profile_link,
        }
    }

    pub fn apply_snapshot(&mut self, snapshot: MessageSnapshot, display_text: String) {
        self.chatter_id = canonical_chatter_id(
            &snapshot.header.provider,
            snapshot.header.user_id.as_deref(),
            &snapshot.header.sender,
        );
        self.provider_id = snapshot.header.provider.clone();
        self.sender = snapshot.header.sender.clone();
        self.user_id = snapshot.header.user_id.clone();
        self.chatter_login = metadata_string(&snapshot, "chatter_login");
        self.badges = metadata_string_array(&snapshot, "badges");
        self.sender_color = metadata_string(&snapshot, "color");
        self.content = snapshot.header.content.clone();
        self.display_text = display_text;
        self.display_segments = display_segments_from_snapshot(&snapshot);
        self.provider = MessageProvider::from_str(&snapshot.header.provider);
        self.status = MessageStatus::from_state(&snapshot.lifecycle.state);
        self.synthesis_trace = synthesis_trace(&snapshot);
        self.source_timestamp = snapshot.header.source_timestamp;
        self.received_at = snapshot.header.received_at;
        self.profile_link = profile_link_for_snapshot(&snapshot, self.chatter_login.as_deref());
    }

    pub fn status_badge(&self) -> String {
        match &self.status {
            MessageStatus::Error(msg) | MessageStatus::Skipped(msg) => msg.clone(),
            other => other.label().to_string(),
        }
    }
}

#[derive(Clone)]
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

#[derive(Clone)]
pub struct SettingEditorState {
    pub draft_value: String,
    pub original_value: String,
    pub dirty: bool,
    pub validation_error: Option<String>,
}

#[derive(Clone)]
pub struct AdvancedSettingChange {
    pub changed_at: OffsetDateTime,
    pub key: String,
    pub previous_value_json: String,
    pub new_value_json: String,
}

#[derive(Clone)]
pub struct AdvancedImportPreviewChange {
    pub key: String,
    pub previous_value_json: String,
    pub new_value_json: String,
    pub source_before: String,
}

#[derive(Clone, Default)]
pub struct AdvancedImportPreview {
    pub changes: Vec<AdvancedImportPreviewChange>,
    pub skipped_unknown: Vec<String>,
    pub skipped_non_advanced: Vec<String>,
}

#[derive(Clone)]
pub struct ChatterEntry {
    pub chatter_id: String,
    pub provider_id: String,
    pub user_id: Option<String>,
    pub chatter_login: Option<String>,
    pub sender: String,
    pub display_name_override: Option<String>,
    pub voice_override: Option<String>,
    pub notes: Option<String>,
    pub badges: Vec<String>,
    pub message_count: usize,
    pub last_seen_at: OffsetDateTime,
    pub is_moderator: bool,
    pub is_channel_owner: bool,
    pub profile_link: Option<String>,
}

impl ChatterEntry {
    pub fn effective_display_name(&self) -> String {
        self.display_name_override
            .clone()
            .unwrap_or_else(|| self.sender.clone())
    }

    pub fn has_voice_override(&self) -> bool {
        self.voice_override
            .as_deref()
            .map(str::trim)
            .map(|value| !value.is_empty())
            .unwrap_or(false)
    }
}

pub struct AppState {
    pub input_value: String,
    pub messages: Vec<MessageEntry>,
    pub message_index: HashMap<u64, usize>,
    pub selected_message_id: Option<u64>,
    pub busy: bool,
    pub status_message: Option<String>,
    pub twitch_status: TwitchStatus,
    pub youtube_status: YouTubeStatus,
    pub provider_statuses: Vec<RuntimeTtsProviderStatus>,
    pub settings_entries: Vec<SettingDisplay>,
    pub settings_editors: HashMap<String, SettingEditorState>,
    pub settings_error: Option<String>,
    pub settings_status_message: Option<String>,
    pub twitch_channel_login_draft: String,
    pub youtube_live_chat_id_draft: String,
    pub youtube_api_key_draft: String,
    pub openai_api_key_draft: String,
    pub show_advanced_open_confirmation: bool,
    pub advanced_change_history: Vec<AdvancedSettingChange>,
    pub advanced_reset_key: String,
    pub advanced_export_snapshot_json: String,
    pub advanced_import_snapshot_draft: String,
    pub advanced_import_preview: Option<AdvancedImportPreview>,
    pub orphan_count: usize,
    pub active_tab: Tab,
    pub history_limit: usize,
    pub playback_enabled: bool,
    pub active_playing_id: Option<u64>,
    pub sticky_to_latest: bool,
    pub hidden_message_count: usize,
    pub show_stop_confirmation: bool,
    pub show_quit_confirmation: bool,
    pub quit_after_stop: bool,
    pub should_quit: bool,
    pub help_mode_enabled: bool,
    pub show_shortcut_catalog: bool,
    pub chatter_entries: Vec<ChatterEntry>,
    pub chatter_search_draft: String,
    pub chatter_role_filter: ChatterRoleFilter,
    pub chatter_override_filter: ChatterOverrideFilter,
    pub selected_chatter_id: Option<String>,
    pub chatter_voice_draft: String,
    pub chatter_display_name_draft: String,
    pub chatter_notes_draft: String,
    pub chatter_status_message: Option<String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            input_value: String::new(),
            messages: Vec::new(),
            message_index: HashMap::new(),
            selected_message_id: None,
            busy: false,
            status_message: None,
            twitch_status: TwitchStatus::Idle,
            youtube_status: YouTubeStatus::Idle,
            provider_statuses: Vec::new(),
            settings_entries: Vec::new(),
            settings_editors: HashMap::new(),
            settings_error: None,
            settings_status_message: None,
            twitch_channel_login_draft: String::new(),
            youtube_live_chat_id_draft: String::new(),
            youtube_api_key_draft: String::new(),
            openai_api_key_draft: String::new(),
            show_advanced_open_confirmation: false,
            advanced_change_history: Vec::new(),
            advanced_reset_key: String::new(),
            advanced_export_snapshot_json: String::new(),
            advanced_import_snapshot_draft: String::new(),
            advanced_import_preview: None,
            orphan_count: 0,
            active_tab: Tab::ChatLogs,
            history_limit: HISTORY_LIMIT,
            playback_enabled: true,
            active_playing_id: None,
            sticky_to_latest: true,
            hidden_message_count: 0,
            show_stop_confirmation: false,
            show_quit_confirmation: false,
            quit_after_stop: false,
            should_quit: false,
            help_mode_enabled: false,
            show_shortcut_catalog: false,
            chatter_entries: Vec::new(),
            chatter_search_draft: String::new(),
            chatter_role_filter: ChatterRoleFilter::All,
            chatter_override_filter: ChatterOverrideFilter::All,
            selected_chatter_id: None,
            chatter_voice_draft: String::new(),
            chatter_display_name_draft: String::new(),
            chatter_notes_draft: String::new(),
            chatter_status_message: None,
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

    pub fn push_advanced_change(&mut self, change: AdvancedSettingChange) {
        self.advanced_change_history.insert(0, change);
        if self.advanced_change_history.len() > ADVANCED_CHANGE_HISTORY_LIMIT {
            self.advanced_change_history
                .truncate(ADVANCED_CHANGE_HISTORY_LIMIT);
        }
    }

    pub fn set_chat_log_sticky_to_latest(&mut self, sticky: bool) {
        if self.sticky_to_latest == sticky {
            return;
        }

        self.sticky_to_latest = sticky;
        if sticky {
            self.hidden_message_count = 0;
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
        self.status_message = Some("Playback continues.".to_string());
    }

    pub fn reduce_stop_cancel(&mut self) {
        self.show_stop_confirmation = false;
        self.quit_after_stop = false;
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
        self.status_message = Some("Quit cancelled.".to_string());
    }

    pub fn reduce_quit_requested(&mut self) -> bool {
        if self.has_pending_playback() {
            self.show_quit_confirmation = true;
            self.show_stop_confirmation = false;
            self.quit_after_stop = false;
            self.status_message = Some("Quit requested while playback is active.".to_string());
            true
        } else {
            self.should_quit = true;
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

fn metadata_string(snapshot: &MessageSnapshot, key: &str) -> Option<String> {
    snapshot
        .header
        .metadata
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn metadata_string_array(snapshot: &MessageSnapshot, key: &str) -> Vec<String> {
    snapshot
        .header
        .metadata
        .get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn display_segments_from_snapshot(snapshot: &MessageSnapshot) -> Vec<MessageDisplaySegment> {
    let Some(raw_segments) = snapshot
        .header
        .metadata
        .get("display_segments")
        .and_then(Value::as_array)
    else {
        return vec![MessageDisplaySegment::Text(
            display_seed(snapshot).to_string(),
        )];
    };

    let mut segments = Vec::new();
    for segment in raw_segments {
        let Some(kind) = segment.get("kind").and_then(Value::as_str) else {
            continue;
        };
        match kind {
            "emote" => {
                let code = segment
                    .get("code")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string)
                    .or_else(|| {
                        segment
                            .get("text")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(ToString::to_string)
                    });
                let Some(code) = code else {
                    continue;
                };
                let url = segment
                    .get("url")
                    .and_then(Value::as_str)
                    .and_then(normalize_display_emote_url);
                segments.push(MessageDisplaySegment::Emote { code, url });
            }
            _ => {
                let Some(text) = segment
                    .get("text")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                else {
                    continue;
                };
                if !text.is_empty() {
                    segments.push(MessageDisplaySegment::Text(text));
                }
            }
        }
    }

    if segments.is_empty() {
        vec![MessageDisplaySegment::Text(
            display_seed(snapshot).to_string(),
        )]
    } else {
        segments
    }
}

fn profile_link_for_snapshot(
    snapshot: &MessageSnapshot,
    chatter_login: Option<&str>,
) -> Option<String> {
    match snapshot.header.provider.as_str() {
        "twitch_chat" => chatter_login.map(|login| format!("https://www.twitch.tv/{login}")),
        "youtube_live_chat" => snapshot
            .header
            .user_id
            .as_ref()
            .map(|channel| format!("https://www.youtube.com/channel/{channel}")),
        _ => None,
    }
}

fn normalize_display_emote_url(raw_url: &str) -> Option<String> {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.starts_with(EMOTE_ASSET_ROUTE_PREFIX) {
        return Some(trimmed.to_string());
    }

    if let Ok(url) = Url::parse(trimmed) {
        if url.scheme() == "file"
            && let Ok(path) = url.to_file_path()
            && let Some(file_name) = path.file_name().and_then(|name| name.to_str())
        {
            let key = file_name.split('.').next().unwrap_or(file_name);
            if !key.is_empty() {
                return Some(format!("{EMOTE_ASSET_ROUTE_PREFIX}{key}"));
            }
        }

        if url.scheme() == "lupo-emote" {
            let mut requested = url.path().trim_start_matches('/').to_string();
            if requested.is_empty()
                && let Some(host) = url.host_str()
            {
                requested = host.to_string();
            }
            let key = requested.split('.').next().unwrap_or(requested.as_str());
            if !key.is_empty() {
                return Some(format!("{EMOTE_ASSET_ROUTE_PREFIX}{key}"));
            }
        }
    }

    Some(trimmed.to_string())
}

fn display_seed(snapshot: &MessageSnapshot) -> &str {
    snapshot
        .header
        .metadata
        .get("display_text")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(match &snapshot.lifecycle.state {
            MessageState::Transformed(state) => state.text.as_str(),
            _ => snapshot.header.content.as_str(),
        })
}

fn synthesis_trace(snapshot: &MessageSnapshot) -> Option<String> {
    let backend = synthesis_backend(snapshot);
    let attempt = snapshot
        .header
        .metadata
        .get("tts_attempt")
        .and_then(Value::as_u64);
    let parameters = snapshot
        .header
        .metadata
        .get("tts_parameters")
        .filter(|value| value.is_object())
        .and_then(|value| serde_json::to_string(value).ok())
        .filter(|json| json != "{}");
    let input_text = snapshot
        .header
        .metadata
        .get("tts_input_text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);

    if backend.is_none() && attempt.is_none() && parameters.is_none() && input_text.is_none() {
        return None;
    }

    let mut summary = String::from("TTS: ");
    if let Some(backend) = backend {
        summary.push_str(&backend);
    } else {
        summary.push_str("unknown backend");
    }

    if let Some(attempt) = attempt {
        summary.push_str(&format!(" (attempt {attempt})"));
    }

    if let Some(parameters) = parameters {
        summary.push_str(" • ");
        summary.push_str(&parameters);
    }

    if let Some(input_text) = input_text {
        summary.push_str(" • input: ");
        summary.push_str(&input_text);
    }

    Some(summary)
}

fn synthesis_backend(snapshot: &MessageSnapshot) -> Option<String> {
    match &snapshot.lifecycle.state {
        MessageState::Synthesizing(state) => state.backend.as_ref().map(label_backend),
        MessageState::Synthesized(state) => Some(label_backend(&state.backend)),
        MessageState::Queued(_) | MessageState::Playing(_) | MessageState::Played(_) => snapshot
            .artifacts
            .get("synth")
            .map(|artifact| match artifact {
                Artifact::Audio(audio) => label_backend(&audio.backend),
            }),
        _ => snapshot
            .header
            .metadata
            .get("tts_backend_label")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| {
                snapshot
                    .header
                    .metadata
                    .get("tts_backend")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            }),
    }
}

fn label_backend(backend: &SynthBackendId) -> String {
    match backend.as_str() {
        "system_tts" => "System TTS".to_string(),
        "openai" => "OpenAI".to_string(),
        "parler" => "Parler".to_string(),
        "ultra_light" => "Ultra-light".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lupo_persistence::{
        ArtifactSet, DeliveryKind, Lifecycle, MessageHeader, MessageSnapshot, TransformingState,
    };
    use serde_json::json;
    use std::collections::BTreeMap;
    use time::OffsetDateTime;

    fn queued_entry() -> MessageEntry {
        MessageEntry {
            id: 1,
            chatter_id: "manual:sender:alice".to_string(),
            provider_id: "manual".to_string(),
            sender: "alice".to_string(),
            user_id: None,
            chatter_login: None,
            badges: Vec::new(),
            sender_color: None,
            content: "hello".to_string(),
            display_text: "hello".to_string(),
            display_segments: vec![MessageDisplaySegment::Text("hello".to_string())],
            provider: MessageProvider::Manual,
            status: MessageStatus::Queued,
            synthesis_trace: None,
            source_timestamp: None,
            received_at: OffsetDateTime::now_utc(),
            profile_link: None,
        }
    }

    #[test]
    fn reduce_quit_requested_shows_confirmation_when_playback_pending() {
        let mut state = AppState::default();
        state.messages.push(queued_entry());

        let should_intercept = state.reduce_quit_requested();
        assert!(should_intercept);
        assert!(state.show_quit_confirmation);
        assert!(!state.quit_after_stop);
    }

    #[test]
    fn reduce_stop_continue_clears_stop_prompt_state() {
        let mut state = AppState {
            show_stop_confirmation: true,
            quit_after_stop: true,
            ..AppState::default()
        };

        state.reduce_stop_continue();
        assert!(!state.show_stop_confirmation);
        assert!(!state.quit_after_stop);
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

    #[test]
    fn push_advanced_change_enforces_limit() {
        let mut state = AppState::default();

        for index in 0..(ADVANCED_CHANGE_HISTORY_LIMIT + 5) {
            state.push_advanced_change(AdvancedSettingChange {
                changed_at: OffsetDateTime::now_utc(),
                key: format!("advanced.key.{index}"),
                previous_value_json: "{}".to_string(),
                new_value_json: "{\"enabled\":true}".to_string(),
            });
        }

        assert_eq!(
            state.advanced_change_history.len(),
            ADVANCED_CHANGE_HISTORY_LIMIT
        );
        assert_eq!(
            state
                .advanced_change_history
                .first()
                .map(|entry| entry.key.as_str()),
            Some("advanced.key.104")
        );
    }

    #[test]
    fn set_chat_log_sticky_to_latest_clears_hidden_counter_when_rejoining_latest() {
        let mut state = AppState {
            sticky_to_latest: false,
            hidden_message_count: 4,
            ..AppState::default()
        };

        state.set_chat_log_sticky_to_latest(true);

        assert!(state.sticky_to_latest);
        assert_eq!(state.hidden_message_count, 0);
    }

    #[test]
    fn set_chat_log_sticky_to_latest_keeps_hidden_counter_when_scrolled_up() {
        let mut state = AppState {
            sticky_to_latest: true,
            hidden_message_count: 2,
            ..AppState::default()
        };

        state.set_chat_log_sticky_to_latest(false);

        assert!(!state.sticky_to_latest);
        assert_eq!(state.hidden_message_count, 2);
    }

    #[test]
    fn message_provider_recognizes_youtube_and_local_aliases() {
        assert_eq!(
            MessageProvider::from_str("youtube_live_chat"),
            MessageProvider::YouTube
        );
        assert_eq!(MessageProvider::from_str("local"), MessageProvider::Manual);
        assert_eq!(MessageProvider::YouTube.label(), "YouTube");
        assert_eq!(MessageProvider::Manual.label(), "Local");
    }

    #[test]
    fn normalize_display_emote_url_maps_file_urls_to_asset_route() {
        let normalized =
            normalize_display_emote_url("file:///tmp/emotes/aabbccddeeff00112233.webp");
        assert_eq!(normalized.as_deref(), Some("/emote/aabbccddeeff00112233"));
    }

    #[test]
    fn normalize_display_emote_url_maps_legacy_scheme_to_asset_route() {
        let normalized = normalize_display_emote_url("lupo-emote:///aabbccddeeff00112233.webp");
        assert_eq!(normalized.as_deref(), Some("/emote/aabbccddeeff00112233"));
    }

    #[test]
    fn message_entry_from_snapshot_reads_metadata_fidelity_fields() {
        let received_at = OffsetDateTime::now_utc();
        let source_at = received_at - time::Duration::seconds(5);
        let mut metadata = BTreeMap::new();
        metadata.insert("display_text".to_string(), json!("Kappa hi"));
        metadata.insert(
            "display_segments".to_string(),
            json!([
                {"kind":"emote","code":"Kappa","url":"https://cdn.example/kappa.png"},
                {"kind":"text","text":" hi"}
            ]),
        );
        metadata.insert("badges".to_string(), json!(["moderator:1"]));
        metadata.insert("chatter_login".to_string(), json!("alice"));
        metadata.insert("color".to_string(), json!("#33cc99"));

        let snapshot = MessageSnapshot {
            header: MessageHeader {
                id: 22,
                provider: "twitch_chat".to_string(),
                provider_message_id: Some("msg-1".to_string()),
                channel_id: Some("chan-1".to_string()),
                user_id: Some("42".to_string()),
                sender: "Alice".to_string(),
                content: "plain".to_string(),
                source_timestamp: Some(source_at),
                received_at,
                delivery_kind: DeliveryKind::Standard,
                payment: None,
                metadata,
            },
            lifecycle: Lifecycle::new(MessageState::Arrived),
            artifacts: ArtifactSet::default(),
        };

        let entry = MessageEntry::from_snapshot(snapshot, "Kappa hi".to_string());
        assert_eq!(entry.provider, MessageProvider::Twitch);
        assert_eq!(entry.badges, vec!["moderator:1".to_string()]);
        assert_eq!(entry.sender_color.as_deref(), Some("#33cc99"));
        assert_eq!(
            entry.profile_link.as_deref(),
            Some("https://www.twitch.tv/alice")
        );
        assert_eq!(entry.source_timestamp, Some(source_at));
        assert_eq!(entry.display_segments.len(), 2);
    }

    #[test]
    fn synthesis_trace_includes_tts_input_text() {
        let received_at = OffsetDateTime::now_utc();
        let mut metadata = BTreeMap::new();
        metadata.insert("tts_input_text".to_string(), json!("hello world from tts"));

        let snapshot = MessageSnapshot {
            header: MessageHeader {
                id: 99,
                provider: "manual".to_string(),
                provider_message_id: None,
                channel_id: None,
                user_id: None,
                sender: "tester".to_string(),
                content: "hello".to_string(),
                source_timestamp: None,
                received_at,
                delivery_kind: DeliveryKind::Standard,
                payment: None,
                metadata,
            },
            lifecycle: Lifecycle::new(MessageState::Transforming(TransformingState {
                started_at: received_at,
                worker: None,
            })),
            artifacts: ArtifactSet::default(),
        };

        let trace = synthesis_trace(&snapshot).expect("trace should be present");
        assert!(trace.contains("input: hello world from tts"));
    }
}

// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use async_channel::unbounded;
use lupo_configuration::{
    SettingDomain, SettingSource, SettingValueKind, SettingsService,
    SettingsSnapshot as ConfigSettingsSnapshot, SnapshotApplyOptions, SnapshotReport,
};
use lupo_persistence::{
    ChatterOverrideRecord, DatabaseHandle, MessageSnapshot, MessageState, app_project_dirs,
    delete_chatter_override, list_chatter_overrides, open_database_at_path, save_chatter_override,
};
use lupo_secrets::{delete_secret, store_secret};
use messages::EmojiResolver;
use serde_json::{Value, json};
use tokio::runtime::Runtime;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tracing::warn;
use unicode_segmentation::UnicodeSegmentation;

use super::action::{AppAction, ShortcutAction, UiAction, UiEvent};
use super::state::{self, AppState, MessageEntry, Tab};
use crate::runtime::bridge::{
    ActionResult, RuntimeBridge, RuntimeError, RuntimeErrorKind, RuntimeEvent, RuntimeIssue,
};
use crate::runtime::pipeline::{
    MessagePipeline, PipelineControlEvent, PipelineEvent, PipelinePlaybackState,
    RuntimeTtsProviderStatus,
};
use crate::runtime::twitch::{
    TwitchConnectorConfig, TwitchConnectorContext, TwitchEvent, TwitchStatus,
};
use crate::runtime::youtube::{
    YouTubeConnectorConfig, YouTubeConnectorContext, YouTubeEvent, YouTubeStatus,
};
use crate::runtime::{AppServices, logging, twitch, youtube};

const OPENAI_SECRET_KEY: &str = "secret_openai_api_key";
const YOUTUBE_API_KEY_SECRET_KEY: &str = "secret_youtube_api_key";
const MAX_RUNTIME_EVENTS_PER_POLL: usize = 128;

pub struct AppModel {
    services: AppServices,
    bridge: RuntimeBridge,
    state: AppState,
    twitch_listener_tasks: Option<TwitchListenerTasks>,
    youtube_listener_tasks: Option<YouTubeListenerTasks>,
}

struct TwitchListenerTasks {
    connector: JoinHandle<()>,
    forwarder: JoinHandle<()>,
}

impl TwitchListenerTasks {
    fn abort(self) {
        self.connector.abort();
        self.forwarder.abort();
    }
}

struct YouTubeListenerTasks {
    connector: JoinHandle<()>,
    forwarder: JoinHandle<()>,
}

impl YouTubeListenerTasks {
    fn abort(self) {
        self.connector.abort();
        self.forwarder.abort();
    }
}

impl AppModel {
    pub fn new() -> Self {
        let bridge = RuntimeBridge::default();
        let services = bootstrap_services();

        spawn_pipeline_listener(&services, bridge.clone());

        let mut this = Self {
            services,
            bridge,
            state: AppState::default(),
            twitch_listener_tasks: None,
            youtube_listener_tasks: None,
        };

        this.restart_twitch_listener();
        this.restart_youtube_listener();

        this.load_initial_messages();
        let playback_state = this.services.pipeline().playback_state_blocking();
        this.handle_playback_state_changed(playback_state);
        this.load_settings_listing();
        this.apply_audio_output_gain_from_settings();
        this.rebuild_chatter_directory();

        if this.state.messages.is_empty() {
            this.state.status_message = Some("Queue is empty.".to_string());
        }

        this
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    pub fn needs_runtime_poll(&self) -> bool {
        self.state.should_quit || self.bridge.has_pending()
    }

    pub fn dispatch(&mut self, action: AppAction) {
        match action {
            AppAction::Event(event) => self.handle_ui_event(event),
            AppAction::TabSelected(tab) => {
                self.select_tab(tab);
            }
            AppAction::ChatLogScrollPositionChanged { at_bottom } => {
                self.handle_chat_log_scroll_position_changed(at_bottom);
            }
            AppAction::InputChanged(value) => {
                self.state.input_value = value;
            }
            AppAction::Trigger(trigger) => match trigger {
                UiAction::QueueMessage => self.submit_manual_message(false),
                UiAction::PlayNow => self.submit_manual_message(true),
                UiAction::TogglePlayback => {
                    if self.state.playback_enabled {
                        if self.state.has_pending_playback() {
                            self.state.show_stop_confirmation = true;
                            self.state.quit_after_stop = false;
                            self.state.status_message = Some("Confirm stop playback.".to_string());
                        } else {
                            self.set_playback_enabled(false);
                        }
                    } else {
                        self.set_playback_enabled(true);
                    }
                }
                UiAction::ClearQueue => self.clear_queue(),
                UiAction::StopNow => self.stop_now(),
            },
            AppAction::StopContinue => {
                self.state.reduce_stop_continue();
            }
            AppAction::StopCancel => {
                self.state.reduce_stop_cancel();
            }
            AppAction::QuitAnyway => {
                self.state.should_quit = true;
            }
            AppAction::QuitStopBefore => {
                self.state.reduce_quit_stop_before();
            }
            AppAction::QuitCancel => {
                self.state.reduce_quit_cancel();
            }
            AppAction::ReloadSettings => {
                self.load_settings_listing();
            }
            AppAction::SettingDraftChanged { key, draft } => {
                self.handle_setting_draft_changed(&key, draft);
            }
            AppAction::SaveSetting(key) => {
                self.save_setting_draft(&key);
            }
            AppAction::ResetSetting(key) => {
                self.reset_setting_value(&key);
            }
            AppAction::DiscardSetting(key) => {
                self.discard_setting_draft(&key);
            }
            AppAction::ReconnectTwitch => {
                self.reconnect_twitch();
            }
            AppAction::RevokeTwitchCredentials => {
                self.revoke_twitch_credentials();
            }
            AppAction::TwitchChannelLoginDraftChanged(draft) => {
                self.handle_twitch_channel_login_draft_changed(draft);
            }
            AppAction::SaveTwitchChannelLogin => {
                self.save_twitch_channel_login();
            }
            AppAction::ClearTwitchChannelLogin => {
                self.clear_twitch_channel_login();
            }
            AppAction::ReconnectYouTube => {
                self.reconnect_youtube();
            }
            AppAction::YouTubeLiveChatIdDraftChanged(draft) => {
                self.handle_youtube_live_chat_id_draft_changed(draft);
            }
            AppAction::SaveYouTubeLiveChatId => {
                self.save_youtube_live_chat_id();
            }
            AppAction::ClearYouTubeLiveChatId => {
                self.clear_youtube_live_chat_id();
            }
            AppAction::YouTubeApiKeyDraftChanged(draft) => {
                self.handle_youtube_api_key_draft_changed(draft);
            }
            AppAction::SaveYouTubeApiKey => {
                self.save_youtube_api_key();
            }
            AppAction::RevokeYouTubeApiKey => {
                self.revoke_youtube_api_key();
            }
            AppAction::OpenAiApiKeyDraftChanged(draft) => {
                self.handle_openai_api_key_draft_changed(draft);
            }
            AppAction::SaveOpenAiApiKey => {
                self.save_openai_api_key();
            }
            AppAction::RevokeOpenAiApiKey => {
                self.revoke_openai_api_key();
            }
            AppAction::EnableAdvanced => {
                self.confirm_advanced_open();
            }
            AppAction::CancelAdvancedOpen => {
                self.cancel_advanced_open();
            }
            AppAction::AdvancedResetKeySelected(key) => {
                self.handle_advanced_reset_key_selected(key);
            }
            AppAction::ResetSelectedAdvancedKey => {
                self.reset_selected_advanced_key();
            }
            AppAction::ResetAllAdvancedSettings => {
                self.reset_all_advanced_settings();
            }
            AppAction::ExportAdvancedSnapshot => {
                self.export_advanced_snapshot();
            }
            AppAction::AdvancedImportSnapshotDraftChanged(draft) => {
                self.handle_advanced_import_snapshot_draft_changed(draft);
            }
            AppAction::PreviewAdvancedImportSnapshot => {
                self.preview_advanced_import_snapshot();
            }
            AppAction::ApplyAdvancedImportSnapshot => {
                self.apply_advanced_import_snapshot();
            }
            AppAction::ChatRowSelected(message_id) => {
                self.state.selected_message_id = Some(message_id);
            }
            AppAction::ReplayMessage(message_id) => {
                self.replay_message(message_id);
            }
            AppAction::SkipMessage(message_id) => {
                self.skip_message(message_id);
            }
            AppAction::OpenShortcutCatalog => {
                self.state.show_shortcut_catalog = true;
            }
            AppAction::CloseShortcutCatalog => {
                self.state.show_shortcut_catalog = false;
            }
            AppAction::ManualNotImplemented => {
                self.state.status_message =
                    Some("Embedded manual is not implemented yet.".to_string());
            }
            AppAction::SetStatusMessage(message) => {
                self.state.status_message = Some(message);
            }
            AppAction::ChatterSearchDraftChanged(draft) => {
                self.state.chatter_search_draft = draft;
            }
            AppAction::ChatterRoleFilterChanged(value) => {
                self.state.chatter_role_filter = state::ChatterRoleFilter::from_ui(&value);
            }
            AppAction::ChatterOverrideFilterChanged(value) => {
                self.state.chatter_override_filter = state::ChatterOverrideFilter::from_ui(&value);
            }
            AppAction::ChatterSelected(chatter_id) => {
                self.select_chatter(&chatter_id);
            }
            AppAction::ChatterVoiceDraftChanged(draft) => {
                self.state.chatter_voice_draft = draft;
            }
            AppAction::ChatterDisplayNameDraftChanged(draft) => {
                self.state.chatter_display_name_draft = draft;
            }
            AppAction::ChatterNotesDraftChanged(draft) => {
                self.state.chatter_notes_draft = draft;
            }
            AppAction::SaveSelectedChatterOverride => {
                self.save_selected_chatter_override();
            }
            AppAction::ResetSelectedChatterOverride => {
                self.reset_selected_chatter_override();
            }
            AppAction::RequestQuit => {
                self.request_quit_intercept();
            }
            AppAction::Shortcut(shortcut) => self.apply_shortcut(shortcut),
        }
    }

    pub fn take_should_quit(&mut self) -> bool {
        std::mem::take(&mut self.state.should_quit)
    }

    fn apply_shortcut(&mut self, shortcut: ShortcutAction) {
        match shortcut {
            ShortcutAction::OpenSettings => {
                self.select_tab(Tab::Settings);
                self.state.status_message = Some("Opened Settings (shortcut).".to_string());
            }
            ShortcutAction::TogglePlayback => {
                self.dispatch(AppAction::Trigger(UiAction::TogglePlayback))
            }
            ShortcutAction::SkipCurrent => self.skip_active_message(),
            ShortcutAction::VolumeUp => self.adjust_output_gain_db(1.0),
            ShortcutAction::VolumeDown => self.adjust_output_gain_db(-1.0),
            ShortcutAction::ToggleHelpMode => {
                self.state.help_mode_enabled = !self.state.help_mode_enabled;
                if self.state.help_mode_enabled {
                    self.state.show_shortcut_catalog = true;
                }
                self.state.status_message = Some(if self.state.help_mode_enabled {
                    "Help mode enabled. Click controls to inspect purpose and workflow.".to_string()
                } else {
                    "Help mode disabled.".to_string()
                });
            }
            ShortcutAction::ExitHelpMode => {
                if self.state.help_mode_enabled {
                    self.state.help_mode_enabled = false;
                    self.state.show_shortcut_catalog = false;
                    self.state.status_message = Some("Help mode disabled.".to_string());
                }
            }
            ShortcutAction::Quit => self.dispatch(AppAction::RequestQuit),
        }
    }

    fn handle_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::PollRuntime => self.process_runtime_events(),
            UiEvent::Runtime(runtime_event) => self.handle_runtime_event(runtime_event),
        }
    }

    fn process_runtime_events(&mut self) {
        let events = self.bridge.drain_up_to(MAX_RUNTIME_EVENTS_PER_POLL);
        for event in events {
            self.handle_ui_event(UiEvent::Runtime(event));
        }
    }

    fn handle_runtime_event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::Pipeline(event) => match event {
                PipelineEvent::SnapshotUpdated(snapshot) => self.apply_snapshot(snapshot),
                PipelineEvent::Control(control_event) => self.handle_control_event(control_event),
                PipelineEvent::PlaybackStateChanged(playback_state) => {
                    self.handle_playback_state_changed(playback_state)
                }
                PipelineEvent::ProviderStatusesChanged(statuses) => {
                    self.handle_provider_statuses_changed(statuses)
                }
                PipelineEvent::Cleared => self.handle_queue_cleared(),
            },
            RuntimeEvent::Twitch(event) => self.handle_twitch_event(event),
            RuntimeEvent::YouTube(event) => self.handle_youtube_event(event),
            RuntimeEvent::ActionResult(result) => self.handle_action_result(result),
            RuntimeEvent::Issue(issue) => {
                self.state.status_message = Some(issue.user_message());
            }
        }
    }

    fn load_initial_messages(&mut self) {
        let mut snapshots = self.services.pipeline().snapshots_blocking();
        snapshots.sort_by(|a, b| a.header.id.cmp(&b.header.id));
        self.state.messages = snapshots
            .into_iter()
            .map(|snapshot| {
                let display = sanitize_for_display(display_seed(&snapshot));
                MessageEntry::from_snapshot(snapshot, display)
            })
            .collect();
        self.state.rebuild_index();
        self.state.trim_history();
        self.state.active_playing_id = self
            .state
            .messages
            .iter()
            .find(|entry| matches!(entry.status, state::MessageStatus::Playing))
            .map(|entry| entry.id);
    }

    fn apply_snapshot(&mut self, snapshot: MessageSnapshot) {
        let message_id = snapshot.header.id;
        let existed = self.state.message_index.contains_key(&message_id);
        let was_sticky = self.state.sticky_to_latest;
        let display = sanitize_for_display(display_seed(&snapshot));

        if let Some(index) = self.state.message_index.get(&message_id).copied() {
            if let Some(entry) = self.state.messages.get_mut(index) {
                entry.apply_snapshot(snapshot, display);
            }
        } else {
            let entry = MessageEntry::from_snapshot(snapshot, display);
            self.state.messages.push(entry);
            self.state.rebuild_index();
            self.state.trim_history();
            if !was_sticky {
                self.state.hidden_message_count = self.state.hidden_message_count.saturating_add(1);
            }
        }

        self.state.active_playing_id = self
            .state
            .messages
            .iter()
            .find(|entry| matches!(entry.status, state::MessageStatus::Playing))
            .map(|entry| entry.id);

        if was_sticky {
            self.state.sticky_to_latest = true;
            self.state.hidden_message_count = 0;
        }

        if !existed && matches!(self.state.active_tab, Tab::ChatLogs) {
            self.state.status_message = Some("Message queued.".to_string());
        }

        self.rebuild_chatter_directory();
    }

    fn handle_playback_state_changed(&mut self, playback_state: PipelinePlaybackState) {
        self.state.playback_enabled = playback_state.enabled;
        self.state.active_playing_id = playback_state.active_message_id;
    }

    fn handle_provider_statuses_changed(&mut self, statuses: Vec<RuntimeTtsProviderStatus>) {
        self.state.provider_statuses = statuses;
    }

    fn handle_control_event(&mut self, event: PipelineControlEvent) {
        match event {
            PipelineControlEvent::ChatCommand(command) => {
                if matches!(self.state.active_tab, Tab::ChatLogs) {
                    let label = if command.command.is_empty() {
                        "Command".to_string()
                    } else {
                        format!("Command: {}", command.command)
                    };
                    self.state.status_message = Some(label);
                }
            }
        }
    }

    fn handle_queue_cleared(&mut self) {
        self.state.messages.clear();
        self.state.message_index.clear();
        self.state.selected_message_id = None;
        self.state.busy = false;
        self.state.active_playing_id = None;
        self.state.hidden_message_count = 0;
        self.state.sticky_to_latest = true;
        self.state.show_stop_confirmation = false;
        self.state.quit_after_stop = false;
        self.state.status_message = Some("Queue cleared.".to_string());
        self.rebuild_chatter_directory();
    }

    fn handle_twitch_event(&mut self, event: TwitchEvent) {
        match event {
            TwitchEvent::Status(status) => {
                match &status {
                    TwitchStatus::Connected { .. } => {}
                    TwitchStatus::CredentialsMissing => {
                        self.state.status_message = Some(
                            "Twitch not configured. Run `lupo-cli twitch auth login`.".to_string(),
                        );
                    }
                    TwitchStatus::Error(message) => {
                        self.state.status_message = Some(format!("Twitch error: {message}"));
                    }
                    TwitchStatus::Disconnected => {
                        self.state.status_message = Some("Twitch connection closed.".to_string());
                    }
                    _ => {}
                }
                self.state.twitch_status = status;
            }
            TwitchEvent::Message(message) => {
                if matches!(self.state.active_tab, Tab::ChatLogs) {
                    let preview = sanitize_for_display(&message.preview);
                    self.state.status_message =
                        Some(format!("New Twitch message queued: {preview}"));
                }
            }
        }
    }

    fn handle_youtube_event(&mut self, event: YouTubeEvent) {
        match event {
            YouTubeEvent::Status(status) => {
                match &status {
                    YouTubeStatus::Connected { .. } => {}
                    YouTubeStatus::CredentialsMissing => {
                        self.state.status_message = Some(
                            "YouTube not configured. Set a live chat ID and API key in Settings -> Connections."
                                .to_string(),
                        );
                    }
                    YouTubeStatus::Error(message) => {
                        self.state.status_message = Some(format!("YouTube error: {message}"));
                    }
                    YouTubeStatus::Disconnected => {
                        self.state.status_message = Some("YouTube connection closed.".to_string());
                    }
                    _ => {}
                }
                self.state.youtube_status = status;
            }
            YouTubeEvent::Message(message) => {
                if matches!(self.state.active_tab, Tab::ChatLogs) {
                    let preview = sanitize_for_display(&message.preview);
                    self.state.status_message =
                        Some(format!("New YouTube message queued: {preview}"));
                }
            }
        }
    }

    fn handle_action_result(&mut self, result: ActionResult) {
        match result {
            ActionResult::SubmitManual {
                play_now,
                submit_result,
                ensure_result,
            } => {
                self.state.busy = false;
                match submit_result {
                    Ok(()) => {
                        if play_now {
                            match ensure_result {
                                Some(Ok(())) => {
                                    self.state.status_message =
                                        Some("Message queued and playback nudged.".to_string());
                                }
                                Some(Err(err)) => {
                                    self.state.status_message = Some(err.user_message());
                                }
                                None => {
                                    self.state.status_message = Some(
                                        "Message queued but playback nudge failed.".to_string(),
                                    );
                                }
                            }
                        } else {
                            self.state.status_message = Some("Message queued.".to_string());
                        }
                    }
                    Err(err) => {
                        self.state.status_message = Some(err.user_message());
                    }
                }
            }
            ActionResult::PlaybackEnabled { enabled, result } => {
                self.state.busy = false;
                match result {
                    Ok(()) => {
                        self.state.playback_enabled = enabled;
                        self.state.status_message = Some(if enabled {
                            "Playback resumed.".to_string()
                        } else {
                            "Playback paused.".to_string()
                        });
                    }
                    Err(err) => {
                        self.state.status_message = Some(err.user_message());
                    }
                }
            }
            ActionResult::StopNow {
                should_quit,
                result,
            } => {
                self.state.busy = false;
                self.state.quit_after_stop = false;
                match result {
                    Ok(()) => {
                        self.state.playback_enabled = false;
                        self.state.status_message = Some("Playback stopped.".to_string());
                        if should_quit {
                            self.state.should_quit = true;
                        }
                    }
                    Err(err) => {
                        self.state.status_message = Some(err.user_message());
                    }
                }
            }
            ActionResult::ClearQueue { result } => match result {
                Ok(()) => self.handle_queue_cleared(),
                Err(err) => {
                    self.state.busy = false;
                    self.state.status_message = Some(err.user_message());
                }
            },
            ActionResult::SkipMessage { message_id, result } => {
                self.state.busy = false;
                match result {
                    Ok(()) => {
                        self.state.status_message = Some(format!("Skipped message #{message_id}."));
                    }
                    Err(err) => {
                        self.state.status_message = Some(err.user_message());
                    }
                }
            }
            ActionResult::ReplayMessage { message_id, result } => {
                self.state.busy = false;
                match result {
                    Ok(()) => {
                        self.state.status_message =
                            Some(format!("Requeued message #{message_id}."));
                    }
                    Err(err) => {
                        self.state.status_message = Some(err.user_message());
                    }
                }
            }
        }
    }

    fn submit_manual_message(&mut self, play_now: bool) {
        let trimmed = self.state.input_value.trim().to_string();
        if trimmed.is_empty() {
            return;
        }

        self.state.input_value.clear();
        self.state.busy = true;
        self.state.status_message = Some("Queuing message...".to_string());

        let services = self.services.clone();
        let bridge = self.bridge.clone();
        services.runtime().spawn(async move {
            let submit_result = services
                .pipeline()
                .submit_manual_message("Operator", trimmed.clone())
                .await
                .map(|_| ())
                .map_err(|err| RuntimeError::new(RuntimeErrorKind::SubmitManual, err.to_string()));

            let ensure_result = if play_now {
                Some(services.pipeline().ensure_all().await.map_err(|err| {
                    RuntimeError::new(RuntimeErrorKind::EnsurePlayback, err.to_string())
                }))
            } else {
                None
            };

            bridge.push(RuntimeEvent::ActionResult(ActionResult::SubmitManual {
                play_now,
                submit_result,
                ensure_result,
            }));
        });
    }

    fn set_playback_enabled(&mut self, enabled: bool) {
        self.state.busy = true;
        self.state.status_message = Some(if enabled {
            "Resuming playback...".to_string()
        } else {
            "Pausing playback...".to_string()
        });

        let services = self.services.clone();
        let bridge = self.bridge.clone();
        services.runtime().spawn(async move {
            let result = services
                .pipeline()
                .set_playback_enabled(enabled)
                .await
                .map_err(|err| {
                    RuntimeError::new(RuntimeErrorKind::SetPlaybackEnabled, err.to_string())
                });

            bridge.push(RuntimeEvent::ActionResult(ActionResult::PlaybackEnabled {
                enabled,
                result,
            }));
        });
    }

    fn stop_now(&mut self) {
        self.state.busy = true;
        self.state.show_stop_confirmation = false;
        self.state.status_message = Some("Stopping playback now...".to_string());
        let should_quit = self.state.quit_after_stop;

        let services = self.services.clone();
        let bridge = self.bridge.clone();
        services.runtime().spawn(async move {
            let result = services
                .pipeline()
                .stop_now()
                .await
                .map_err(|err| RuntimeError::new(RuntimeErrorKind::StopNow, err.to_string()));

            bridge.push(RuntimeEvent::ActionResult(ActionResult::StopNow {
                should_quit,
                result,
            }));
        });
    }

    fn clear_queue(&mut self) {
        if self.state.busy {
            return;
        }

        self.state.busy = true;
        self.state.status_message = Some("Clearing queue...".to_string());

        let services = self.services.clone();
        let bridge = self.bridge.clone();
        services.runtime().spawn(async move {
            let result =
                services.pipeline().clear_all().await.map_err(|err| {
                    RuntimeError::new(RuntimeErrorKind::ClearQueue, err.to_string())
                });
            bridge.push(RuntimeEvent::ActionResult(ActionResult::ClearQueue {
                result,
            }));
        });
    }

    fn skip_message(&mut self, message_id: u64) {
        self.state.busy = true;
        self.state.status_message = Some(format!("Skipping message #{message_id}..."));
        let services = self.services.clone();
        let bridge = self.bridge.clone();
        services.runtime().spawn(async move {
            let result = services
                .pipeline()
                .skip_message(message_id)
                .await
                .map_err(|err| RuntimeError::new(RuntimeErrorKind::SkipMessage, err.to_string()));
            bridge.push(RuntimeEvent::ActionResult(ActionResult::SkipMessage {
                message_id,
                result,
            }));
        });
    }

    fn replay_message(&mut self, message_id: u64) {
        self.state.busy = true;
        self.state.status_message = Some(format!("Replaying message #{message_id}..."));
        let services = self.services.clone();
        let bridge = self.bridge.clone();
        services.runtime().spawn(async move {
            let result = services
                .pipeline()
                .retry_message(message_id)
                .await
                .map_err(|err| RuntimeError::new(RuntimeErrorKind::ReplayMessage, err.to_string()));
            bridge.push(RuntimeEvent::ActionResult(ActionResult::ReplayMessage {
                message_id,
                result,
            }));
        });
    }

    fn skip_active_message(&mut self) {
        let Some(message_id) = self.state.active_playing_id else {
            self.state.status_message = Some("No active message is currently playing.".to_string());
            return;
        };
        self.skip_message(message_id);
    }

    fn adjust_output_gain_db(&mut self, delta_db: f64) {
        let current_db = current_audio_gain_db(&self.state.settings_entries);
        let target_db = (current_db + delta_db).clamp(-60.0, 6.0);
        if (target_db - current_db).abs() < f64::EPSILON {
            return;
        }

        if let Err(err) = self.persist_audio_gain_db(target_db) {
            self.state.settings_error = Some(err);
            return;
        }

        let linear = db_to_linear_gain(target_db);
        match self.services.pipeline().set_output_volume_blocking(linear) {
            Ok(applied) => {
                self.state.settings_error = None;
                self.state.status_message = Some(format!(
                    "Output gain: {target_db:.1} dB ({:.0}%).",
                    applied * 100.0
                ));
            }
            Err(err) => {
                self.state.settings_error =
                    Some(format!("Failed to apply output gain to audio mixer: {err}"));
            }
        }
    }

    fn request_quit_intercept(&mut self) {
        self.state.reduce_quit_requested();
    }

    fn handle_chat_log_scroll_position_changed(&mut self, at_bottom: bool) {
        self.state.set_chat_log_sticky_to_latest(at_bottom);
    }

    fn select_tab(&mut self, tab: Tab) {
        if tab == Tab::Advanced {
            self.request_advanced_open();
            return;
        }

        self.state.active_tab = tab;
        self.state.show_advanced_open_confirmation = false;
    }

    fn request_advanced_open(&mut self) {
        if self.state.active_tab == Tab::Advanced {
            return;
        }

        self.state.show_advanced_open_confirmation = true;
        self.state.status_message = Some("Confirm opening Advanced Settings.".to_string());
    }

    fn confirm_advanced_open(&mut self) {
        self.state.show_advanced_open_confirmation = false;
        self.state.active_tab = Tab::Advanced;
        self.state.status_message = Some("Advanced Settings opened for this session.".to_string());
    }

    fn cancel_advanced_open(&mut self) {
        self.state.show_advanced_open_confirmation = false;
        self.state.status_message = Some("Advanced Settings open cancelled.".to_string());
    }

    fn advanced_setting_keys(&self) -> Vec<String> {
        self.state
            .settings_entries
            .iter()
            .filter(|entry| entry.domain == SettingDomain::Advanced)
            .map(|entry| entry.key.clone())
            .collect()
    }

    fn sync_advanced_reset_key(&mut self) {
        let keys = self.advanced_setting_keys();
        if keys.is_empty() {
            self.state.advanced_reset_key.clear();
            return;
        }
        if !keys.iter().any(|key| key == &self.state.advanced_reset_key) {
            self.state.advanced_reset_key = keys[0].clone();
        }
    }

    fn handle_advanced_reset_key_selected(&mut self, key: String) {
        if self
            .advanced_setting_keys()
            .iter()
            .any(|known| known == &key)
        {
            self.state.advanced_reset_key = key;
            self.state.settings_error = None;
        } else {
            self.state.settings_error = Some(format!("Unknown advanced key: {key}"));
        }
    }

    fn reset_selected_advanced_key(&mut self) {
        let key = self.state.advanced_reset_key.trim().to_string();
        if key.is_empty() {
            self.state.settings_error = Some("No advanced key selected to reset.".to_string());
            return;
        }
        self.reset_setting_value(&key);
    }

    fn reset_all_advanced_settings(&mut self) {
        let keys = self.advanced_setting_keys();
        if keys.is_empty() {
            self.state.settings_status_message = Some("No advanced keys available.".to_string());
            self.state.settings_error = None;
            return;
        }

        let mut reset_count = 0usize;
        let mut failures = Vec::new();
        for key in keys {
            let previous_value_json = self
                .setting_value_json(&key)
                .unwrap_or_else(|| "null".to_string());

            if let Err(err) = self.services.settings().reset(&key) {
                failures.push(format!("{key}: {err}"));
                continue;
            }
            if let Err(err) = self.refresh_setting_entry(&key) {
                failures.push(format!("{key}: {err}"));
                continue;
            }

            if let Some(new_value_json) = self.setting_value_json(&key) {
                self.record_advanced_setting_change(&key, previous_value_json, new_value_json);
            }
            reset_count += 1;
        }

        self.sync_advanced_reset_key();

        if failures.is_empty() {
            let runtime_note = self.apply_runtime_effects_for_setting_change("advanced");
            self.state.settings_status_message = Some(match runtime_note {
                Some(note) => {
                    format!("Reset {reset_count} advanced setting(s) to defaults. {note}")
                }
                None => format!("Reset {reset_count} advanced setting(s) to defaults."),
            });
            self.state.settings_error = None;
        } else {
            self.state.settings_error = Some(format!(
                "Failed to reset some advanced settings: {}",
                failures.join(" | ")
            ));
        }
    }

    fn export_advanced_snapshot(&mut self) {
        let snapshot = match self.services.settings().snapshot() {
            Ok(snapshot) => snapshot,
            Err(err) => {
                self.state.settings_error = Some(format!("Failed to export snapshot: {err}"));
                return;
            }
        };

        let advanced_keys: HashSet<String> = self.advanced_setting_keys().into_iter().collect();
        let filtered = snapshot
            .settings
            .into_iter()
            .filter(|(key, _)| advanced_keys.contains(key))
            .collect::<BTreeMap<_, _>>();

        let export_snapshot = ConfigSettingsSnapshot {
            generated_at: snapshot.generated_at,
            settings: filtered,
        };
        let rendered = match serde_json::to_string_pretty(&export_snapshot) {
            Ok(json) => json,
            Err(err) => {
                self.state.settings_error =
                    Some(format!("Failed to render exported snapshot: {err}"));
                return;
            }
        };

        self.state.advanced_export_snapshot_json = rendered;
        self.state.settings_status_message =
            Some("Exported advanced settings snapshot.".to_string());
        self.state.settings_error = None;
    }

    fn handle_advanced_import_snapshot_draft_changed(&mut self, draft: String) {
        self.state.advanced_import_snapshot_draft = draft;
        self.state.advanced_import_preview = None;
        self.state.settings_error = None;
    }

    fn preview_advanced_import_snapshot(&mut self) {
        let (snapshot, skipped_non_advanced) = match self.parse_advanced_snapshot_draft() {
            Ok(parsed) => parsed,
            Err(err) => {
                self.state.settings_error = Some(err);
                return;
            }
        };

        let report = match self.services.settings().apply_snapshot(
            &snapshot,
            SnapshotApplyOptions {
                allow_unknown: true,
                dry_run: true,
            },
        ) {
            Ok(report) => report,
            Err(err) => {
                self.state.settings_error = Some(format!("Failed to preview snapshot: {err}"));
                return;
            }
        };

        let preview = build_advanced_import_preview(report, skipped_non_advanced);
        let change_count = preview.changes.len();
        self.state.advanced_import_preview = Some(preview);
        self.state.settings_status_message = Some(format!(
            "Previewed advanced import: {change_count} change(s) detected."
        ));
        self.state.settings_error = None;
    }

    fn apply_advanced_import_snapshot(&mut self) {
        if self.state.advanced_import_preview.is_none() {
            self.state.settings_error =
                Some("Run preview before applying advanced import.".to_string());
            return;
        }

        let (snapshot, skipped_non_advanced) = match self.parse_advanced_snapshot_draft() {
            Ok(parsed) => parsed,
            Err(err) => {
                self.state.settings_error = Some(err);
                return;
            }
        };

        let report = match self.services.settings().apply_snapshot(
            &snapshot,
            SnapshotApplyOptions {
                allow_unknown: true,
                dry_run: false,
            },
        ) {
            Ok(report) => report,
            Err(err) => {
                self.state.settings_error = Some(format!("Failed to apply snapshot: {err}"));
                return;
            }
        };

        let history_changes = report
            .applied
            .iter()
            .map(|change| {
                (
                    change.key.clone(),
                    format_json_value(&change.previous_value),
                    format_json_value(&change.new_value),
                )
            })
            .collect::<Vec<_>>();
        for (key, previous_value_json, new_value_json) in history_changes {
            self.record_advanced_setting_change(&key, previous_value_json, new_value_json);
        }

        self.state.advanced_import_preview =
            Some(build_advanced_import_preview(report, skipped_non_advanced));
        self.load_settings_listing();
        let runtime_note = self.apply_runtime_effects_for_setting_change("advanced");
        self.state.settings_status_message = Some(match runtime_note {
            Some(note) => format!("Applied advanced snapshot import. {note}"),
            None => "Applied advanced snapshot import.".to_string(),
        });
        self.state.settings_error = None;
    }

    fn parse_advanced_snapshot_draft(
        &self,
    ) -> Result<(ConfigSettingsSnapshot, Vec<String>), String> {
        let trimmed = self.state.advanced_import_snapshot_draft.trim();
        if trimmed.is_empty() {
            return Err("Import snapshot JSON cannot be empty.".to_string());
        }

        let parsed = serde_json::from_str::<ConfigSettingsSnapshot>(trimmed)
            .map_err(|err| format!("Invalid snapshot JSON: {err}"))?;

        let advanced_keys: HashSet<String> = self.advanced_setting_keys().into_iter().collect();
        if advanced_keys.is_empty() {
            return Err("No advanced keys are available to import.".to_string());
        }

        let mut filtered = BTreeMap::new();
        let mut skipped_non_advanced = Vec::new();
        for (key, value) in parsed.settings {
            if advanced_keys.contains(&key) {
                filtered.insert(key, value);
            } else {
                skipped_non_advanced.push(key);
            }
        }
        skipped_non_advanced.sort();

        if filtered.is_empty() {
            return Err("Snapshot contains no advanced keys to apply.".to_string());
        }

        Ok((
            ConfigSettingsSnapshot {
                generated_at: parsed.generated_at,
                settings: filtered,
            },
            skipped_non_advanced,
        ))
    }

    fn load_settings_listing(&mut self) {
        match self.services.settings().list() {
            Ok(listing) => {
                self.state.settings_entries = listing
                    .entries
                    .into_iter()
                    .map(state::SettingDisplay::from)
                    .collect();

                self.state.settings_editors.clear();

                for entry in &self.state.settings_entries {
                    self.state.settings_editors.insert(
                        entry.key.clone(),
                        state::SettingEditorState {
                            draft_value: entry.value_json.clone(),
                            original_value: entry.value_json.clone(),
                            dirty: false,
                            validation_error: None,
                        },
                    );
                }

                self.state.settings_error = None;
                self.state.settings_status_message = None;
                self.state.orphan_count = listing.orphans.len();
                self.state.twitch_channel_login_draft =
                    current_twitch_channel_override(&self.state.settings_entries)
                        .unwrap_or_default();
                self.state.youtube_live_chat_id_draft =
                    current_youtube_live_chat_id(&self.state.settings_entries).unwrap_or_default();
                self.sync_advanced_reset_key();
            }
            Err(err) => {
                self.state.settings_error = Some(format!("Failed to load settings: {err}"));
            }
        }
    }

    fn handle_setting_draft_changed(&mut self, key: &str, draft: String) {
        if let Some(editor) = self.state.settings_editors.get_mut(key) {
            editor.draft_value = draft;
            editor.dirty = editor.draft_value != editor.original_value;
            editor.validation_error = None;
        }
    }

    fn set_setting_validation_error(&mut self, key: &str, message: String) {
        if let Some(editor) = self.state.settings_editors.get_mut(key) {
            editor.validation_error = Some(message);
        } else {
            self.state.settings_error = Some(message);
        }
    }

    fn refresh_setting_entry(&mut self, key: &str) -> Result<(), String> {
        let record = self
            .services
            .settings()
            .get(key)
            .map_err(|err| format!("Failed to load setting '{key}': {err}"))?;
        let display = state::SettingDisplay::from(record);
        let refreshed_json = display.value_json.clone();

        if let Some(index) = self
            .state
            .settings_entries
            .iter()
            .position(|entry| entry.key == key)
        {
            self.state.settings_entries[index] = display;
        } else {
            self.state.settings_entries.push(display);
        }

        if let Some(editor) = self.state.settings_editors.get_mut(key) {
            editor.original_value = refreshed_json.clone();
            editor.draft_value = refreshed_json;
            editor.dirty = false;
            editor.validation_error = None;
        }

        Ok(())
    }

    fn save_setting_draft(&mut self, key: &str) {
        let Some(entry) = self
            .state
            .settings_entries
            .iter()
            .find(|entry| entry.key == key)
        else {
            self.state.settings_error = Some(format!("Unknown setting key: {key}"));
            return;
        };
        let value_kind = entry.value_kind;
        let title = entry.title.clone();
        let previous_value_json = entry.value_json.clone();
        let is_advanced = entry.domain == SettingDomain::Advanced;

        let Some(editor) = self.state.settings_editors.get(key) else {
            self.state.settings_error = Some(format!("Missing editor state for setting '{key}'"));
            return;
        };
        if !editor.dirty {
            self.state.settings_status_message = Some(format!("No unsaved changes for {title}."));
            return;
        }
        let draft = editor.draft_value.clone();
        let parsed = match parse_setting_draft_value(&draft, value_kind) {
            Ok(value) => value,
            Err(err) => {
                self.set_setting_validation_error(key, err);
                return;
            }
        };

        match self.services.settings().set(key, parsed) {
            Ok(()) => match self.refresh_setting_entry(key) {
                Ok(()) => {
                    if is_advanced && let Some(new_value_json) = self.setting_value_json(key) {
                        self.record_advanced_setting_change(
                            key,
                            previous_value_json.clone(),
                            new_value_json,
                        );
                    }
                    let runtime_note = self.apply_runtime_effects_for_setting_change(key);
                    self.state.settings_status_message = Some(match runtime_note {
                        Some(note) => format!("Saved {title}. {note}"),
                        None => format!("Saved {title}."),
                    });
                    self.state.settings_error = None;
                }
                Err(err) => {
                    self.state.settings_error = Some(err);
                }
            },
            Err(err) => {
                self.set_setting_validation_error(key, format!("Failed to save setting: {err}"));
            }
        }
    }

    fn reset_setting_value(&mut self, key: &str) {
        let Some(entry) = self
            .state
            .settings_entries
            .iter()
            .find(|entry| entry.key == key)
        else {
            self.state.settings_error = Some(format!("Unknown setting key: {key}"));
            return;
        };
        let title = entry.title.clone();
        let previous_value_json = entry.value_json.clone();
        let is_advanced = entry.domain == SettingDomain::Advanced;

        match self.services.settings().reset(key) {
            Ok(()) => match self.refresh_setting_entry(key) {
                Ok(()) => {
                    if is_advanced && let Some(new_value_json) = self.setting_value_json(key) {
                        self.record_advanced_setting_change(
                            key,
                            previous_value_json,
                            new_value_json,
                        );
                    }
                    let runtime_note = self.apply_runtime_effects_for_setting_change(key);
                    self.state.settings_status_message = Some(match runtime_note {
                        Some(note) => format!("Reset {title} to default. {note}"),
                        None => format!("Reset {title} to default."),
                    });
                    self.state.settings_error = None;
                }
                Err(err) => self.state.settings_error = Some(err),
            },
            Err(err) => {
                self.set_setting_validation_error(key, format!("Failed to reset setting: {err}"));
            }
        }
    }

    fn discard_setting_draft(&mut self, key: &str) {
        let Some(editor) = self.state.settings_editors.get_mut(key) else {
            self.state.settings_error = Some(format!("Missing editor state for setting '{key}'"));
            return;
        };
        if !editor.dirty {
            self.state.settings_status_message = Some(format!("No unsaved changes for {key}."));
            return;
        }

        let original = editor.original_value.clone();
        editor.draft_value = original;
        editor.dirty = false;
        editor.validation_error = None;
        self.state.settings_status_message = Some(format!("Discarded unsaved changes for {key}."));
    }

    fn setting_value_json(&self, key: &str) -> Option<String> {
        self.state
            .settings_entries
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.value_json.clone())
    }

    fn record_advanced_setting_change(
        &mut self,
        key: &str,
        previous_value_json: String,
        new_value_json: String,
    ) {
        if previous_value_json == new_value_json {
            return;
        }

        self.state
            .push_advanced_change(state::AdvancedSettingChange {
                changed_at: time::OffsetDateTime::now_utc(),
                key: key.to_string(),
                previous_value_json,
                new_value_json,
            });
    }

    fn apply_runtime_effects_for_setting_change(&mut self, key: &str) -> Option<String> {
        match key {
            "connections" => {
                self.restart_twitch_listener();
                self.restart_youtube_listener();
                Some("Restarted Twitch and YouTube connectors.".to_string())
            }
            "audio_output" => {
                let gain_db = current_audio_gain_db(&self.state.settings_entries);
                let gain = db_to_linear_gain(gain_db);
                match self.services.pipeline().set_output_volume_blocking(gain) {
                    Ok(_) => Some("Applied audio output gain to mixer.".to_string()),
                    Err(err) => {
                        self.state.settings_error =
                            Some(format!("Failed to apply audio output gain: {err}"));
                        None
                    }
                }
            }
            _ => runtime_effect_note_for_setting_key(key).map(ToString::to_string),
        }
    }

    fn stop_twitch_listener(&mut self) {
        if let Some(tasks) = self.twitch_listener_tasks.take() {
            tasks.abort();
        }
    }

    fn stop_youtube_listener(&mut self) {
        if let Some(tasks) = self.youtube_listener_tasks.take() {
            tasks.abort();
        }
    }

    fn restart_twitch_listener(&mut self) {
        self.stop_twitch_listener();
        self.state.twitch_status = TwitchStatus::Connecting;
        let config = self.twitch_connector_config();
        self.twitch_listener_tasks = Some(spawn_twitch_listener(
            &self.services,
            self.bridge.clone(),
            config,
        ));
    }

    fn restart_youtube_listener(&mut self) {
        self.stop_youtube_listener();
        self.state.youtube_status = YouTubeStatus::Connecting;
        let config = self.youtube_connector_config();
        self.youtube_listener_tasks = Some(spawn_youtube_listener(
            &self.services,
            self.bridge.clone(),
            config,
        ));
    }

    fn twitch_connector_config(&self) -> TwitchConnectorConfig {
        let override_login = self
            .services
            .settings()
            .get("connections")
            .ok()
            .and_then(|record| parse_connections_channel_override(&record.value));

        TwitchConnectorConfig {
            channel_login: override_login,
        }
    }

    fn youtube_connector_config(&self) -> YouTubeConnectorConfig {
        let live_chat_id = self
            .services
            .settings()
            .get("connections")
            .ok()
            .and_then(|record| parse_connections_youtube_live_chat_id(&record.value));

        YouTubeConnectorConfig { live_chat_id }
    }

    fn handle_twitch_channel_login_draft_changed(&mut self, draft: String) {
        self.state.twitch_channel_login_draft = draft;
        self.state.settings_error = None;
    }

    fn save_twitch_channel_login(&mut self) {
        let normalized = match normalize_channel_login_input(&self.state.twitch_channel_login_draft)
        {
            Ok(value) => value,
            Err(err) => {
                self.state.settings_error = Some(err);
                return;
            }
        };

        if let Err(err) = self.set_connections_channel_override(normalized.clone()) {
            self.state.settings_error = Some(err);
            return;
        }

        let _ = self.refresh_setting_entry("connections");
        self.state.twitch_channel_login_draft = normalized.clone().unwrap_or_default();
        self.restart_twitch_listener();
        self.state.settings_status_message = Some(match normalized {
            Some(login) => {
                format!("Saved Twitch target channel '{login}' and restarted connector.")
            }
            None => "Using connected account channel and restarted connector.".to_string(),
        });
        self.state.settings_error = None;
    }

    fn clear_twitch_channel_login(&mut self) {
        if let Err(err) = self.set_connections_channel_override(None) {
            self.state.settings_error = Some(err);
            return;
        }

        self.state.twitch_channel_login_draft.clear();
        let _ = self.refresh_setting_entry("connections");
        self.restart_twitch_listener();
        self.state.settings_status_message =
            Some("Cleared Twitch target channel override and restarted connector.".to_string());
        self.state.settings_error = None;
    }

    fn set_connections_channel_override(
        &self,
        channel_login: Option<String>,
    ) -> Result<(), String> {
        self.set_connections_string_field("twitch_channel_login", channel_login)
    }

    fn handle_youtube_live_chat_id_draft_changed(&mut self, draft: String) {
        self.state.youtube_live_chat_id_draft = draft;
        self.state.settings_error = None;
    }

    fn save_youtube_live_chat_id(&mut self) {
        let normalized =
            match normalize_youtube_live_chat_id_input(&self.state.youtube_live_chat_id_draft) {
                Ok(value) => value,
                Err(err) => {
                    self.state.settings_error = Some(err);
                    return;
                }
            };

        if let Err(err) = self.set_connections_youtube_live_chat_id(normalized.clone()) {
            self.state.settings_error = Some(err);
            return;
        }

        let _ = self.refresh_setting_entry("connections");
        self.state.youtube_live_chat_id_draft = normalized.clone().unwrap_or_default();
        self.restart_youtube_listener();
        self.state.settings_status_message = Some(match normalized {
            Some(chat_id) => {
                format!("Saved YouTube live chat ID '{chat_id}' and restarted connector.")
            }
            None => "Cleared YouTube live chat ID and restarted connector.".to_string(),
        });
        self.state.settings_error = None;
    }

    fn clear_youtube_live_chat_id(&mut self) {
        if let Err(err) = self.set_connections_youtube_live_chat_id(None) {
            self.state.settings_error = Some(err);
            return;
        }

        self.state.youtube_live_chat_id_draft.clear();
        let _ = self.refresh_setting_entry("connections");
        self.restart_youtube_listener();
        self.state.settings_status_message =
            Some("Cleared YouTube live chat ID and restarted connector.".to_string());
        self.state.settings_error = None;
    }

    fn set_connections_youtube_live_chat_id(
        &self,
        live_chat_id: Option<String>,
    ) -> Result<(), String> {
        self.set_connections_string_field("youtube_live_chat_id", live_chat_id)
    }

    fn set_connections_string_field(
        &self,
        field_key: &str,
        value: Option<String>,
    ) -> Result<(), String> {
        let mut record = self
            .services
            .settings()
            .get("connections")
            .map_err(|err| format!("Failed to load connections setting: {err}"))?;

        let Some(object) = record.value.as_object_mut() else {
            return Err("Connections setting is not a JSON object.".to_string());
        };

        match value {
            Some(value) => {
                object.insert(field_key.to_string(), Value::String(value));
            }
            None => {
                object.insert(field_key.to_string(), Value::Null);
            }
        }

        self.services
            .settings()
            .set("connections", record.value)
            .map_err(|err| format!("Failed to save connections setting: {err}"))
    }

    fn handle_youtube_api_key_draft_changed(&mut self, draft: String) {
        self.state.youtube_api_key_draft = draft;
        self.state.settings_error = None;
    }

    fn save_youtube_api_key(&mut self) {
        let trimmed = self.state.youtube_api_key_draft.trim().to_string();
        if trimmed.is_empty() {
            self.state.settings_error = Some("YouTube API key cannot be empty.".to_string());
            return;
        }

        match store_secret(
            &self.services.settings(),
            YOUTUBE_API_KEY_SECRET_KEY,
            trimmed.as_bytes(),
        ) {
            Ok(()) => {
                self.state.youtube_api_key_draft.clear();
                self.load_settings_listing();
                self.restart_youtube_listener();
                self.state.settings_status_message = Some("Saved YouTube API key.".to_string());
                self.state.settings_error = None;
            }
            Err(err) => {
                self.state.settings_error = Some(format!("Failed to save YouTube API key: {err}"));
            }
        }
    }

    fn revoke_youtube_api_key(&mut self) {
        match delete_secret(&self.services.settings(), YOUTUBE_API_KEY_SECRET_KEY) {
            Ok(()) => {
                self.state.youtube_api_key_draft.clear();
                self.load_settings_listing();
                self.restart_youtube_listener();
                self.state.settings_status_message = Some("Revoked YouTube API key.".to_string());
                self.state.settings_error = None;
            }
            Err(err) => {
                self.state.settings_error =
                    Some(format!("Failed to revoke YouTube API key: {err}"));
            }
        }
    }

    fn handle_openai_api_key_draft_changed(&mut self, draft: String) {
        self.state.openai_api_key_draft = draft;
        self.state.settings_error = None;
    }

    fn save_openai_api_key(&mut self) {
        let trimmed = self.state.openai_api_key_draft.trim().to_string();
        if trimmed.is_empty() {
            self.state.settings_error = Some("OpenAI API key cannot be empty.".to_string());
            return;
        }

        match store_secret(
            &self.services.settings(),
            OPENAI_SECRET_KEY,
            trimmed.as_bytes(),
        ) {
            Ok(()) => {
                self.state.openai_api_key_draft.clear();
                self.load_settings_listing();
                self.state.settings_status_message = Some("Saved OpenAI API key.".to_string());
                self.state.settings_error = None;
            }
            Err(err) => {
                self.state.settings_error = Some(format!("Failed to save OpenAI API key: {err}"));
            }
        }
    }

    fn revoke_openai_api_key(&mut self) {
        match delete_secret(&self.services.settings(), OPENAI_SECRET_KEY) {
            Ok(()) => {
                self.state.openai_api_key_draft.clear();
                self.load_settings_listing();
                self.state.settings_status_message = Some("Revoked OpenAI API key.".to_string());
                self.state.settings_error = None;
            }
            Err(err) => {
                self.state.settings_error = Some(format!("Failed to revoke OpenAI API key: {err}"));
            }
        }
    }

    fn reconnect_twitch(&mut self) {
        self.restart_twitch_listener();
        self.state.settings_status_message = Some("Reconnecting Twitch listener...".to_string());
        self.state.settings_error = None;
    }

    fn reconnect_youtube(&mut self) {
        self.restart_youtube_listener();
        self.state.settings_status_message = Some("Reconnecting YouTube listener...".to_string());
        self.state.settings_error = None;
    }

    fn revoke_twitch_credentials(&mut self) {
        self.stop_twitch_listener();
        match twitch::clear_stored_credentials(&self.services.settings()) {
            Ok(()) => {
                self.state.twitch_status = TwitchStatus::CredentialsMissing;
                self.load_settings_listing();
                self.state.settings_status_message =
                    Some("Revoked Twitch credentials.".to_string());
                self.state.settings_error = None;
            }
            Err(err) => {
                self.state.settings_error =
                    Some(format!("Failed to revoke Twitch credentials: {err}"));
            }
        }
    }

    fn apply_audio_output_gain_from_settings(&mut self) {
        let gain_db = current_audio_gain_db(&self.state.settings_entries);
        let linear = db_to_linear_gain(gain_db);
        if let Err(err) = self.services.pipeline().set_output_volume_blocking(linear) {
            self.state.settings_error =
                Some(format!("Failed to initialize audio output gain: {err}"));
        }
    }

    fn persist_audio_gain_db(&mut self, gain_db: f64) -> Result<(), String> {
        let mut record = self
            .services
            .settings()
            .get("audio_output")
            .map_err(|err| format!("Failed to load audio_output setting: {err}"))?;
        let Some(object) = record.value.as_object_mut() else {
            return Err("audio_output setting is not a JSON object.".to_string());
        };
        object.insert("gain_db".to_string(), json!(gain_db));
        self.services
            .settings()
            .set("audio_output", record.value)
            .map_err(|err| format!("Failed to save audio_output setting: {err}"))?;
        self.load_settings_listing();
        Ok(())
    }

    fn rebuild_chatter_directory(&mut self) {
        let overrides = list_chatter_overrides(&self.services.settings().database())
            .unwrap_or_else(|err| {
                self.state.chatter_status_message =
                    Some(format!("Failed to load chatter overrides: {err}"));
                Vec::new()
            });
        self.state.chatter_entries = build_chatter_entries(&self.state.messages, overrides);

        let selected = self.state.selected_chatter_id.clone();
        if let Some(selected) = selected {
            self.select_chatter(&selected);
        } else if let Some(first) = self.state.chatter_entries.first() {
            let first_id = first.chatter_id.clone();
            self.select_chatter(&first_id);
        }
    }

    fn select_chatter(&mut self, chatter_id: &str) {
        let Some(chatter) = self
            .state
            .chatter_entries
            .iter()
            .find(|entry| entry.chatter_id == chatter_id)
        else {
            return;
        };
        self.state.selected_chatter_id = Some(chatter.chatter_id.clone());
        self.state.chatter_voice_draft = chatter.voice_override.clone().unwrap_or_default();
        self.state.chatter_display_name_draft =
            chatter.display_name_override.clone().unwrap_or_default();
        self.state.chatter_notes_draft = chatter.notes.clone().unwrap_or_default();
    }

    fn save_selected_chatter_override(&mut self) {
        let Some(chatter_id) = self.state.selected_chatter_id.clone() else {
            self.state.chatter_status_message = Some("Select a chatter first.".to_string());
            return;
        };
        let Some(chatter) = self
            .state
            .chatter_entries
            .iter()
            .find(|entry| entry.chatter_id == chatter_id)
            .cloned()
        else {
            self.state.chatter_status_message =
                Some("Selected chatter is unavailable.".to_string());
            return;
        };

        let voice = normalize_optional_text(&self.state.chatter_voice_draft);
        let display_name = normalize_optional_text(&self.state.chatter_display_name_draft);
        let notes = normalize_optional_text(&self.state.chatter_notes_draft);

        let db = self.services.settings().database();
        let save_result = if voice.is_none() && display_name.is_none() && notes.is_none() {
            delete_chatter_override(&db, &chatter_id)
        } else {
            let record = ChatterOverrideRecord {
                chatter_id: chatter_id.clone(),
                provider: chatter.provider_id.clone(),
                user_id: chatter.user_id.clone(),
                voice_override: voice,
                display_name_override: display_name,
                notes,
                updated_at: time::OffsetDateTime::now_utc(),
            };
            save_chatter_override(&db, &record)
        };

        match save_result {
            Ok(()) => {
                self.rebuild_chatter_directory();
                self.state.chatter_status_message = Some("Saved chatter override.".to_string());
            }
            Err(err) => {
                self.state.chatter_status_message =
                    Some(format!("Failed to save chatter override: {err}"));
            }
        }
    }

    fn reset_selected_chatter_override(&mut self) {
        let Some(chatter_id) = self.state.selected_chatter_id.clone() else {
            self.state.chatter_status_message = Some("Select a chatter first.".to_string());
            return;
        };
        self.select_chatter(&chatter_id);
        self.state.chatter_status_message = Some("Reset chatter drafts.".to_string());
    }
}

fn bootstrap_services() -> AppServices {
    let dirs = app_project_dirs().expect("missing dirs");
    let log_guard = logging::setup_logging(dirs.log_dir()).expect("failed to set up logging");

    let runtime = Arc::new(Runtime::new().expect("failed to create tokio runtime"));
    let db = open_database_for_services(runtime.clone(), dirs.data_local_dir().to_path_buf())
        .expect("failed to initialize database");
    let audio =
        Arc::new(lupo_audio::Audio::start_default().expect("failed to initialize audio output"));
    let settings = SettingsService::new(db.clone());

    let pipeline =
        MessagePipeline::start(runtime.clone(), db.clone(), audio.clone(), settings.clone());

    AppServices::new(runtime, settings, pipeline, log_guard)
}

fn open_database_for_services(
    runtime: Arc<Runtime>,
    data_dir: std::path::PathBuf,
) -> Result<DatabaseHandle, lupo_persistence::Error> {
    if tokio::runtime::Handle::try_current().is_ok() {
        // Dioxus desktop can already run inside a Tokio runtime. Avoid nested block_on panics by
        // running startup DB init on a dedicated thread.
        std::thread::spawn(move || {
            runtime.block_on(async { open_database_at_path(data_dir).await })
        })
        .join()
        .map_err(|_| {
            lupo_persistence::Error::Storage("database init thread panicked".to_string())
        })?
    } else {
        runtime.block_on(async { open_database_at_path(data_dir).await })
    }
}

fn spawn_pipeline_listener(services: &AppServices, bridge: RuntimeBridge) {
    let pipeline = services.pipeline();
    let runtime = services.runtime();
    runtime.spawn(async move {
        let mut receiver = pipeline.subscribe();

        loop {
            match receiver.recv().await {
                Ok(event) => bridge.push(RuntimeEvent::Pipeline(event)),
                Err(RecvError::Lagged(skipped)) => {
                    warn!(skipped, "pipeline event subscriber lagged; continuing");
                    bridge.push(RuntimeEvent::Issue(RuntimeIssue::PipelineLagged {
                        skipped,
                    }));
                }
                Err(RecvError::Closed) => {
                    bridge.push(RuntimeEvent::Issue(RuntimeIssue::PipelineChannelClosed));
                    break;
                }
            }
        }
    });
}

fn spawn_twitch_listener(
    services: &AppServices,
    bridge: RuntimeBridge,
    config: TwitchConnectorConfig,
) -> TwitchListenerTasks {
    let (tx, rx) = unbounded::<TwitchEvent>();
    let context = TwitchConnectorContext {
        settings: services.settings(),
        pipeline: services.pipeline(),
    };

    let connector = twitch::spawn_listener(services.runtime(), context, tx, config);

    let runtime = services.runtime();
    let forwarder = runtime.spawn(async move {
        while let Ok(event) = rx.recv().await {
            bridge.push(RuntimeEvent::Twitch(event));
        }
    });

    TwitchListenerTasks {
        connector,
        forwarder,
    }
}

fn spawn_youtube_listener(
    services: &AppServices,
    bridge: RuntimeBridge,
    config: YouTubeConnectorConfig,
) -> YouTubeListenerTasks {
    let (tx, rx) = unbounded::<YouTubeEvent>();
    let context = YouTubeConnectorContext {
        settings: services.settings(),
        pipeline: services.pipeline(),
    };

    let connector = youtube::spawn_listener(services.runtime(), context, tx, config);

    let runtime = services.runtime();
    let forwarder = runtime.spawn(async move {
        while let Ok(event) = rx.recv().await {
            bridge.push(RuntimeEvent::YouTube(event));
        }
    });

    YouTubeListenerTasks {
        connector,
        forwarder,
    }
}

fn parse_connections_channel_override(value: &Value) -> Option<String> {
    let login = value.get("twitch_channel_login")?.as_str()?;
    let trimmed = login.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_ascii_lowercase())
    }
}

fn parse_connections_youtube_live_chat_id(value: &Value) -> Option<String> {
    let chat_id = value.get("youtube_live_chat_id")?.as_str()?;
    let trimmed = chat_id.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn normalize_youtube_live_chat_id_input(raw: &str) -> Result<Option<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    if trimmed.len() < 8 {
        return Err("YouTube live chat ID looks too short.".to_string());
    }
    if trimmed.chars().any(|ch| ch.is_whitespace()) {
        return Err("YouTube live chat ID may not contain whitespace.".to_string());
    }

    Ok(Some(trimmed.to_string()))
}

fn normalize_channel_login_input(raw: &str) -> Result<Option<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return Err(
            "Twitch channel login may only contain letters, numbers, and underscore.".to_string(),
        );
    }
    Ok(Some(trimmed.to_ascii_lowercase()))
}

fn current_twitch_channel_override(entries: &[state::SettingDisplay]) -> Option<String> {
    entries
        .iter()
        .find(|entry| entry.key == "connections")
        .and_then(|entry| serde_json::from_str::<Value>(&entry.value_json).ok())
        .and_then(|value| parse_connections_channel_override(&value))
}

fn current_youtube_live_chat_id(entries: &[state::SettingDisplay]) -> Option<String> {
    entries
        .iter()
        .find(|entry| entry.key == "connections")
        .and_then(|entry| serde_json::from_str::<Value>(&entry.value_json).ok())
        .and_then(|value| parse_connections_youtube_live_chat_id(&value))
}

fn current_audio_gain_db(entries: &[state::SettingDisplay]) -> f64 {
    entries
        .iter()
        .find(|entry| entry.key == "audio_output")
        .and_then(|entry| serde_json::from_str::<Value>(&entry.value_json).ok())
        .and_then(|value| value.get("gain_db").and_then(Value::as_f64))
        .unwrap_or(0.0)
}

fn db_to_linear_gain(db: f64) -> f32 {
    10_f64.powf(db / 20.0) as f32
}

fn normalize_optional_text(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn chatter_profile_link(
    provider_id: &str,
    user_id: Option<&str>,
    chatter_login: Option<&str>,
) -> Option<String> {
    match provider_id {
        "twitch_chat" => chatter_login.map(|login| format!("https://www.twitch.tv/{login}")),
        "youtube_live_chat" => {
            user_id.map(|channel| format!("https://www.youtube.com/channel/{channel}"))
        }
        _ => None,
    }
}

fn build_chatter_entries(
    messages: &[state::MessageEntry],
    overrides: Vec<ChatterOverrideRecord>,
) -> Vec<state::ChatterEntry> {
    struct AggregatedChatter {
        chatter_id: String,
        provider_id: String,
        user_id: Option<String>,
        chatter_login: Option<String>,
        sender: String,
        badges: std::collections::BTreeSet<String>,
        message_count: usize,
        last_seen_at: time::OffsetDateTime,
        is_moderator: bool,
        is_channel_owner: bool,
    }

    let overrides_by_id: std::collections::HashMap<String, ChatterOverrideRecord> = overrides
        .into_iter()
        .map(|record| (record.chatter_id.clone(), record))
        .collect();

    let mut aggregate: std::collections::HashMap<String, AggregatedChatter> =
        std::collections::HashMap::new();

    for message in messages {
        let entry = aggregate
            .entry(message.chatter_id.clone())
            .or_insert_with(|| AggregatedChatter {
                chatter_id: message.chatter_id.clone(),
                provider_id: message.provider_id.clone(),
                user_id: message.user_id.clone(),
                chatter_login: message.chatter_login.clone(),
                sender: message.sender.clone(),
                badges: std::collections::BTreeSet::new(),
                message_count: 0,
                last_seen_at: message.source_timestamp.unwrap_or(message.received_at),
                is_moderator: false,
                is_channel_owner: false,
            });
        entry.message_count += 1;
        let seen_at = message.source_timestamp.unwrap_or(message.received_at);
        if seen_at > entry.last_seen_at {
            entry.last_seen_at = seen_at;
        }
        entry.is_moderator |= message
            .badges
            .iter()
            .any(|badge| badge.starts_with("moderator:"));
        entry.is_channel_owner |= message
            .badges
            .iter()
            .any(|badge| badge.starts_with("broadcaster:"));
        for badge in &message.badges {
            entry.badges.insert(badge.clone());
        }
        if entry.chatter_login.is_none() {
            entry.chatter_login = message.chatter_login.clone();
        }
        if entry.user_id.is_none() {
            entry.user_id = message.user_id.clone();
        }
        if entry.sender.is_empty() {
            entry.sender = message.sender.clone();
        }
    }

    for override_id in overrides_by_id.keys() {
        aggregate
            .entry(override_id.clone())
            .or_insert_with(|| AggregatedChatter {
                chatter_id: override_id.clone(),
                provider_id: override_id
                    .split(':')
                    .next()
                    .unwrap_or("manual")
                    .to_string(),
                user_id: None,
                chatter_login: None,
                sender: override_id.clone(),
                badges: std::collections::BTreeSet::new(),
                message_count: 0,
                last_seen_at: time::OffsetDateTime::UNIX_EPOCH,
                is_moderator: false,
                is_channel_owner: false,
            });
    }

    let mut chatters = aggregate
        .into_values()
        .map(|base| {
            let override_row = overrides_by_id.get(&base.chatter_id);
            let display_name_override = override_row
                .and_then(|row| row.display_name_override.as_ref())
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
            let voice_override = override_row
                .and_then(|row| row.voice_override.as_ref())
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
            let notes = override_row
                .and_then(|row| row.notes.as_ref())
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());

            state::ChatterEntry {
                chatter_id: base.chatter_id.clone(),
                provider_id: base.provider_id.clone(),
                user_id: base.user_id.clone(),
                chatter_login: base.chatter_login.clone(),
                sender: base.sender.clone(),
                display_name_override,
                voice_override,
                notes,
                badges: base.badges.into_iter().collect(),
                message_count: base.message_count,
                last_seen_at: base.last_seen_at,
                is_moderator: base.is_moderator,
                is_channel_owner: base.is_channel_owner,
                profile_link: chatter_profile_link(
                    &base.provider_id,
                    base.user_id.as_deref(),
                    base.chatter_login.as_deref(),
                ),
            }
        })
        .collect::<Vec<_>>();

    chatters.sort_by(|a, b| {
        b.last_seen_at
            .cmp(&a.last_seen_at)
            .then_with(|| a.effective_display_name().cmp(&b.effective_display_name()))
    });
    chatters
}

fn build_advanced_import_preview(
    report: SnapshotReport,
    skipped_non_advanced: Vec<String>,
) -> state::AdvancedImportPreview {
    let changes = report
        .applied
        .into_iter()
        .map(|change| state::AdvancedImportPreviewChange {
            key: change.key,
            previous_value_json: format_json_value(&change.previous_value),
            new_value_json: format_json_value(&change.new_value),
            source_before: match change.source_before {
                SettingSource::Default => "default".to_string(),
                SettingSource::Persisted { scope, .. } => format!("persisted ({scope})"),
            },
        })
        .collect();

    state::AdvancedImportPreview {
        changes,
        skipped_unknown: report.skipped_unknown,
        skipped_non_advanced,
    }
}

fn format_json_value(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
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

fn sanitize_for_display(text: &str) -> String {
    let mut sanitized = String::with_capacity(text.len());

    for grapheme in UnicodeSegmentation::graphemes(text, true) {
        if EmojiResolver::is_emoji_grapheme(grapheme) {
            sanitized.push_str("[emoji]");
        } else {
            sanitized.push_str(grapheme);
        }
    }

    let trimmed = sanitized.trim();
    if trimmed.is_empty() {
        "[emoji]".to_string()
    } else {
        trimmed.to_string()
    }
}

fn parse_setting_draft_value(draft: &str, value_kind: SettingValueKind) -> Result<Value, String> {
    let trimmed = draft.trim();
    if trimmed.is_empty() {
        return Err("Value cannot be empty.".to_string());
    }

    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => Ok(value),
        Err(err) => match value_kind {
            SettingValueKind::String | SettingValueKind::Enum { .. } => {
                Ok(Value::String(trimmed.to_string()))
            }
            _ => Err(format!("Invalid JSON value: {err}")),
        },
    }
}

fn runtime_effect_note_for_setting_key(key: &str) -> Option<&'static str> {
    match key {
        "message_filtering" => Some("Applies to newly arrived messages."),
        "command_acceptance" => Some("Applies to newly received commands."),
        "tts" => Some("Applies to subsequent synthesis jobs."),
        "advanced" => Some("Applies to subsequent scheduler checks."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    #[test]
    fn parse_setting_draft_value_rejects_empty() {
        let result = parse_setting_draft_value("  ", SettingValueKind::Number);
        assert!(result.is_err());
    }

    #[test]
    fn parse_setting_draft_value_treats_string_kind_as_literal() {
        let value = parse_setting_draft_value("hello", SettingValueKind::String).unwrap();
        assert_eq!(value, Value::String("hello".to_string()));
    }

    #[test]
    fn sanitize_for_display_preserves_japanese_text() {
        assert_eq!(
            sanitize_for_display("こんにちは、世界"),
            "こんにちは、世界".to_string()
        );
    }

    #[test]
    fn runtime_effect_note_marks_tts_as_live_applied() {
        assert_eq!(
            runtime_effect_note_for_setting_key("tts"),
            Some("Applies to subsequent synthesis jobs.")
        );
    }

    #[test]
    fn open_database_for_services_works_inside_existing_runtime() {
        let outer_runtime = Runtime::new().expect("failed to create outer runtime");
        outer_runtime.block_on(async {
            let inner_runtime = Arc::new(Runtime::new().expect("failed to create inner runtime"));
            let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
            let _db = open_database_for_services(inner_runtime, temp_dir.path().to_path_buf())
                .expect("failed to open database inside existing runtime");
            assert!(
                temp_dir
                    .path()
                    .join(lupo_persistence::DATABASE_FILENAME)
                    .exists()
            );
        });
    }

    #[test]
    fn build_chatter_entries_aggregates_messages_and_applies_override() {
        let now = OffsetDateTime::now_utc();
        let messages = vec![
            test_message_entry(
                1,
                "twitch_chat:user:42",
                "twitch_chat",
                "Alice",
                Some("42"),
                Some("alice"),
                vec!["moderator:1".to_string()],
                now,
            ),
            test_message_entry(
                2,
                "twitch_chat:user:42",
                "twitch_chat",
                "Alice",
                Some("42"),
                Some("alice"),
                vec!["broadcaster:1".to_string()],
                now + time::Duration::seconds(2),
            ),
        ];
        let overrides = vec![ChatterOverrideRecord {
            chatter_id: "twitch_chat:user:42".to_string(),
            provider: "twitch_chat".to_string(),
            user_id: Some("42".to_string()),
            voice_override: Some("nova".to_string()),
            display_name_override: Some("Alice Prime".to_string()),
            notes: Some("trusted".to_string()),
            updated_at: now,
        }];

        let chatters = build_chatter_entries(&messages, overrides);
        assert_eq!(chatters.len(), 1);
        let chatter = &chatters[0];
        assert_eq!(chatter.chatter_id, "twitch_chat:user:42");
        assert_eq!(chatter.message_count, 2);
        assert!(chatter.is_moderator);
        assert!(chatter.is_channel_owner);
        assert_eq!(chatter.voice_override.as_deref(), Some("nova"));
        assert_eq!(
            chatter.display_name_override.as_deref(),
            Some("Alice Prime")
        );
        assert_eq!(chatter.notes.as_deref(), Some("trusted"));
        assert_eq!(
            chatter.profile_link.as_deref(),
            Some("https://www.twitch.tv/alice")
        );
    }

    #[test]
    fn build_chatter_entries_includes_override_only_rows() {
        let now = OffsetDateTime::now_utc();
        let overrides = vec![ChatterOverrideRecord {
            chatter_id: "youtube_live_chat:user:uc123".to_string(),
            provider: "youtube_live_chat".to_string(),
            user_id: Some("uc123".to_string()),
            voice_override: Some("sol".to_string()),
            display_name_override: None,
            notes: None,
            updated_at: now,
        }];

        let chatters = build_chatter_entries(&[], overrides);
        assert_eq!(chatters.len(), 1);
        let chatter = &chatters[0];
        assert_eq!(chatter.provider_id, "youtube_live_chat");
        assert_eq!(chatter.message_count, 0);
        assert_eq!(chatter.voice_override.as_deref(), Some("sol"));
    }

    #[test]
    fn normalize_optional_text_trims_empty_to_none() {
        assert_eq!(normalize_optional_text("  "), None);
        assert_eq!(normalize_optional_text("  hi  "), Some("hi".to_string()));
    }

    #[test]
    fn chatter_profile_link_builds_provider_urls() {
        assert_eq!(
            chatter_profile_link("twitch_chat", None, Some("alice")),
            Some("https://www.twitch.tv/alice".to_string())
        );
        assert_eq!(
            chatter_profile_link("youtube_live_chat", Some("UC123"), None),
            Some("https://www.youtube.com/channel/UC123".to_string())
        );
        assert_eq!(chatter_profile_link("manual", None, None), None);
    }

    #[allow(clippy::too_many_arguments)]
    fn test_message_entry(
        id: u64,
        chatter_id: &str,
        provider_id: &str,
        sender: &str,
        user_id: Option<&str>,
        chatter_login: Option<&str>,
        badges: Vec<String>,
        seen_at: OffsetDateTime,
    ) -> state::MessageEntry {
        state::MessageEntry {
            id,
            chatter_id: chatter_id.to_string(),
            provider_id: provider_id.to_string(),
            sender: sender.to_string(),
            user_id: user_id.map(ToString::to_string),
            chatter_login: chatter_login.map(ToString::to_string),
            badges,
            sender_color: None,
            content: "hello".to_string(),
            display_text: "hello".to_string(),
            display_segments: vec![state::MessageDisplaySegment::Text("hello".to_string())],
            provider: state::MessageProvider::Twitch,
            status: state::MessageStatus::Queued,
            synthesis_trace: None,
            source_timestamp: Some(seen_at),
            received_at: seen_at,
            profile_link: None,
        }
    }
}

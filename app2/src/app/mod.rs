// SPDX-License-Identifier: Apache-2.0

mod message;
mod state;
mod tabs;

use std::sync::Arc;
use std::time::Duration;

use async_channel::unbounded;
use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Element, Font, Length, Subscription, Task, window};
use lupo_configuration::{SettingValueKind, SettingsService};
use lupo_persistence::{MessageSnapshot, MessageState, app_project_dirs, open_database_at_path};
use messages::EmojiResolver;
use serde_json::Value;
use tokio::runtime::Runtime;
use tokio::sync::broadcast::error::RecvError;
use tracing::warn;
use unicode_segmentation::UnicodeSegmentation;

use self::message::{AppMessage, UiAction, UiEvent};
use self::state::{AppState, MessageEntry, Tab};
use crate::runtime::bridge::{
    ActionResult, RuntimeBridge, RuntimeError, RuntimeErrorKind, RuntimeEvent, RuntimeIssue,
};
use crate::runtime::pipeline::{
    MessagePipeline, PipelineControlEvent, PipelineEvent, PipelinePlaybackState,
};
use crate::runtime::twitch::{
    TwitchConnectorConfig, TwitchConnectorContext, TwitchEvent, TwitchStatus,
};
use crate::runtime::{AppServices, logging, twitch};

pub fn run() {
    let app = iced::application(LupoIcedApp::new, update, view)
        .title("Lupo (Iced Trial)")
        .default_font(preferred_default_font())
        .subscription(subscription);
    if let Err(error) = app.run() {
        eprintln!("failed to run lupo2: {error}");
    }
}

#[cfg(target_os = "macos")]
fn preferred_default_font() -> Font {
    Font::with_name("Hiragino Sans")
}

#[cfg(not(target_os = "macos"))]
fn preferred_default_font() -> Font {
    Font::default()
}

struct LupoIcedApp {
    services: AppServices,
    bridge: RuntimeBridge,
    state: AppState,
}

impl LupoIcedApp {
    fn new() -> Self {
        let bridge = RuntimeBridge::default();
        let services = bootstrap_services();

        spawn_pipeline_listener(&services, bridge.clone());
        spawn_twitch_listener(&services, bridge.clone());

        let mut this = Self {
            services,
            bridge,
            state: AppState::default(),
        };

        this.load_initial_messages();
        let playback_state = this.services.pipeline().playback_state_blocking();
        this.handle_playback_state_changed(playback_state);
        this.load_settings_listing();

        if this.state.messages.is_empty() {
            this.state.status_message = Some("Queue is empty.".to_string());
        }

        this
    }

    fn update(&mut self, message: AppMessage) -> Task<AppMessage> {
        match message {
            AppMessage::Event(event) => return self.handle_ui_event(event),
            AppMessage::TabSelected(tab) => {
                self.state.active_tab = tab;
            }
            AppMessage::InputChanged(value) => {
                self.state.input_value = value;
            }
            AppMessage::Trigger(action) => match action {
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
            AppMessage::StopContinue => {
                self.state.reduce_stop_continue();
            }
            AppMessage::StopCancel => {
                self.state.reduce_stop_cancel();
            }
            AppMessage::QuitAnyway => {
                return self.close_pending_or_latest_window();
            }
            AppMessage::QuitStopBefore => {
                self.state.reduce_quit_stop_before();
            }
            AppMessage::QuitCancel => {
                self.state.reduce_quit_cancel();
            }
            AppMessage::ReloadSettings => {
                self.load_settings_listing();
            }
            AppMessage::SettingDraftChanged { key, draft } => {
                self.handle_setting_draft_changed(&key, draft);
            }
            AppMessage::SaveSetting(key) => {
                self.save_setting_draft(&key);
            }
            AppMessage::ResetSetting(key) => {
                self.reset_setting_value(&key);
            }
            AppMessage::DiscardSetting(key) => {
                self.discard_setting_draft(&key);
            }
            AppMessage::EnableAdvanced => {
                self.state.show_advanced = true;
                self.state.active_tab = Tab::Advanced;
            }
            AppMessage::MessageListScrolled(offset) => {
                self.state.message_scroll_offset = offset;
                self.state.update_sticky_state_from_scroll();
            }
            AppMessage::RequestQuit => {
                if !self.request_quit_intercept() {
                    return self.resolve_latest_window_close();
                }
            }
        }

        Task::none()
    }

    fn view(&self) -> Element<'_, AppMessage> {
        let nav = row![
            nav_button(Tab::ChatLogs, self.state.active_tab, "Chat Logs"),
            nav_button(Tab::Chatters, self.state.active_tab, "Chatters"),
            nav_button(Tab::Settings, self.state.active_tab, "Settings"),
            nav_button(Tab::Advanced, self.state.active_tab, "Advanced Settings"),
            button(text("Quit")).on_press(AppMessage::RequestQuit),
        ]
        .spacing(8)
        .padding(12);

        let body = match self.state.active_tab {
            Tab::ChatLogs => tabs::chat_logs::view(&self.state),
            Tab::Chatters => tabs::chatters::view(),
            Tab::Settings => scrollable(tabs::settings::view(&self.state)).into(),
            Tab::Advanced => scrollable(tabs::advanced::view(&self.state)).into(),
        };

        container(
            column![nav, body]
                .width(Length::Fill)
                .height(Length::Fill)
                .spacing(8),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }
}

fn update(app: &mut LupoIcedApp, message: AppMessage) -> Task<AppMessage> {
    app.update(message)
}

fn view(app: &LupoIcedApp) -> Element<'_, AppMessage> {
    app.view()
}

fn subscription(_app: &LupoIcedApp) -> Subscription<AppMessage> {
    Subscription::batch(vec![
        iced::time::every(Duration::from_millis(100))
            .map(|_| AppMessage::Event(UiEvent::PollRuntime)),
        window::close_requests().map(|id| AppMessage::Event(UiEvent::WindowCloseRequested(id))),
    ])
}

fn nav_button(
    tab: Tab,
    active_tab: Tab,
    label: &'static str,
) -> iced::widget::Button<'static, AppMessage> {
    if tab == active_tab {
        button(text(format!("• {label}"))).on_press(AppMessage::TabSelected(tab))
    } else {
        button(text(label)).on_press(AppMessage::TabSelected(tab))
    }
}

fn bootstrap_services() -> AppServices {
    let dirs = app_project_dirs().expect("missing dirs");
    let log_guard = logging::setup_logging(dirs.log_dir()).expect("failed to set up logging");

    let runtime = Arc::new(Runtime::new().expect("failed to create tokio runtime"));
    let db = runtime
        .block_on(async { open_database_at_path(dirs.data_local_dir()).await })
        .expect("failed to initialize database");
    let audio =
        Arc::new(lupo_audio::Audio::start_default().expect("failed to initialize audio output"));
    let settings = SettingsService::new(db.clone());

    let pipeline =
        MessagePipeline::start(runtime.clone(), db.clone(), audio.clone(), settings.clone());

    AppServices::new(runtime, settings, pipeline, log_guard)
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

fn spawn_twitch_listener(services: &AppServices, bridge: RuntimeBridge) {
    let (tx, rx) = unbounded::<TwitchEvent>();
    let context = TwitchConnectorContext {
        settings: services.settings(),
        pipeline: services.pipeline(),
    };

    twitch::spawn_listener(
        services.runtime(),
        context,
        tx,
        TwitchConnectorConfig::default(),
    );

    let runtime = services.runtime();
    runtime.spawn(async move {
        while let Ok(event) = rx.recv().await {
            bridge.push(RuntimeEvent::Twitch(event));
        }
        bridge.push(RuntimeEvent::Issue(RuntimeIssue::TwitchChannelClosed));
    });
}

impl LupoIcedApp {
    fn handle_ui_event(&mut self, event: UiEvent) -> Task<AppMessage> {
        match event {
            UiEvent::PollRuntime => self.process_runtime_events(),
            UiEvent::Runtime(runtime_event) => self.handle_runtime_event(runtime_event),
            UiEvent::WindowCloseRequested(window_id) => {
                if self.request_quit_intercept_with_window(Some(window_id)) {
                    Task::none()
                } else {
                    window::close::<AppMessage>(window_id).discard()
                }
            }
            UiEvent::ResolveWindowClose(window_id) => {
                if let Some(window_id) = window_id {
                    window::close::<AppMessage>(window_id).discard()
                } else {
                    self.state.status_message =
                        Some("No active window available to close.".to_string());
                    Task::none()
                }
            }
        }
    }

    fn process_runtime_events(&mut self) -> Task<AppMessage> {
        let events = self.bridge.drain();
        let mut tasks = Vec::new();
        for event in events {
            tasks.push(self.handle_ui_event(UiEvent::Runtime(event)));
        }
        Task::batch(tasks)
    }

    fn handle_runtime_event(&mut self, event: RuntimeEvent) -> Task<AppMessage> {
        match event {
            RuntimeEvent::Pipeline(event) => match event {
                PipelineEvent::SnapshotUpdated(snapshot) => self.apply_snapshot(snapshot),
                PipelineEvent::Control(control_event) => self.handle_control_event(control_event),
                PipelineEvent::PlaybackStateChanged(playback_state) => {
                    self.handle_playback_state_changed(playback_state)
                }
                PipelineEvent::Cleared => self.handle_queue_cleared(),
            },
            RuntimeEvent::Twitch(event) => self.handle_twitch_event(event),
            RuntimeEvent::ActionResult(result) => return self.handle_action_result(result),
            RuntimeEvent::Issue(issue) => {
                self.state.status_message = Some(issue.user_message());
            }
        }
        Task::none()
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
    }

    fn handle_playback_state_changed(&mut self, playback_state: PipelinePlaybackState) {
        self.state.playback_enabled = playback_state.enabled;
        self.state.active_playing_id = playback_state.active_message_id;
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
        self.state.busy = false;
        self.state.active_playing_id = None;
        self.state.hidden_message_count = 0;
        self.state.sticky_to_latest = true;
        self.state.show_stop_confirmation = false;
        self.state.quit_after_stop = false;
        self.state.status_message = Some("Queue cleared.".to_string());
    }

    fn handle_twitch_event(&mut self, event: TwitchEvent) {
        match event {
            TwitchEvent::Status(status) => {
                match &status {
                    TwitchStatus::Connected { channel } => {
                        self.state.status_message =
                            Some(format!("Connected to Twitch channel {channel}."));
                    }
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

    fn handle_action_result(&mut self, result: ActionResult) -> Task<AppMessage> {
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
                window_id,
                result,
            } => {
                self.state.busy = false;
                self.state.quit_after_stop = false;
                match result {
                    Ok(()) => {
                        self.state.playback_enabled = false;
                        self.state.status_message = Some("Playback stopped.".to_string());
                        if should_quit {
                            self.state.pending_quit_window_id = None;
                            if let Some(window_id) = window_id {
                                return window::close::<AppMessage>(window_id).discard();
                            }
                            return self.resolve_latest_window_close();
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
        }
        Task::none()
    }

    fn submit_manual_message(&mut self, play_now: bool) {
        let trimmed = self.state.input_value.trim().to_string();
        if trimmed.is_empty() {
            return;
        }

        self.state.input_value.clear();
        self.state.busy = true;
        self.state.status_message = Some("Queuing message…".to_string());

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
            "Resuming playback…".to_string()
        } else {
            "Pausing playback…".to_string()
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
        self.state.status_message = Some("Stopping playback now…".to_string());
        let should_quit = self.state.quit_after_stop;
        let window_id = self.state.pending_quit_window_id;

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
                window_id,
                result,
            }));
        });
    }

    fn clear_queue(&mut self) {
        if self.state.busy {
            return;
        }

        self.state.busy = true;
        self.state.status_message = Some("Clearing queue…".to_string());

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

    fn request_quit_intercept(&mut self) -> bool {
        self.request_quit_intercept_with_window(None)
    }

    fn request_quit_intercept_with_window(&mut self, window_id: Option<window::Id>) -> bool {
        self.state.reduce_quit_requested(window_id)
    }

    fn close_pending_or_latest_window(&mut self) -> Task<AppMessage> {
        if let Some(window_id) = self.state.pending_quit_window_id.take() {
            window::close::<AppMessage>(window_id).discard()
        } else {
            self.resolve_latest_window_close()
        }
    }

    fn resolve_latest_window_close(&self) -> Task<AppMessage> {
        window::latest().map(|id| AppMessage::Event(UiEvent::ResolveWindowClose(id)))
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
                    self.state.settings_status_message = Some(format!("Saved {title}."));
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

        match self.services.settings().reset(key) {
            Ok(()) => match self.refresh_setting_entry(key) {
                Ok(()) => {
                    self.state.settings_status_message = Some(format!("Reset {title} to default."));
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
}

fn display_seed(snapshot: &MessageSnapshot) -> &str {
    match &snapshot.lifecycle.state {
        MessageState::Transformed(state) => state.text.as_str(),
        _ => snapshot.header.content.as_str(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn sticky_state_resets_hidden_counter_at_latest() {
        let mut state = AppState {
            hidden_message_count: 2,
            message_scroll_offset: 1.0,
            ..AppState::default()
        };
        state.update_sticky_state_from_scroll();

        assert!(state.sticky_to_latest);
        assert_eq!(state.hidden_message_count, 0);
    }

    #[test]
    fn sanitize_for_display_preserves_japanese_text() {
        assert_eq!(
            sanitize_for_display("こんにちは、世界"),
            "こんにちは、世界".to_string()
        );
    }
}

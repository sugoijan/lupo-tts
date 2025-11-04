// SPDX-License-Identifier: Apache-2.0
#![allow(
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::large_enum_variant,
    clippy::redundant_guards,
    clippy::too_many_arguments
)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Cursor;
use std::sync::Arc;

use hex::encode as hex_encode;
use hound::WavReader;
use lupo_audio::Audio;
use lupo_configuration::SettingsService;
use lupo_persistence::{
    Artifact, AudioArtifact, AudioFormat, DeliveryKind, ErrorState, MessageHeader, MessageSnapshot,
    MessageState, MessageStateKind, NewMessage, PipelineStage, PlayedState, PlayingState,
    QueuePriority, QueuedState, SkipReason, SkippedState, SynthBackendId, SynthesizedState,
    SynthesizingState, TransformedState, TransformingState, TransitionCause, clear_all_messages,
    delete_app_state_json, enqueue_message, list_messages_desc, load_app_state_json,
    load_audio_artifact, save_app_state_json, save_message_snapshot, store_audio_artifact,
};
use lupo_secrets::load_secret_string;
use lupo_tts_openai::OpenAi;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::runtime::Runtime;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, oneshot};
use tracing::{debug, error, warn};

const FILTERING_QUEUE_SIZE: usize = 128;
const TRANSFORMATION_QUEUE_SIZE: usize = 128;
const SYNTHESIS_QUEUE_SIZE: usize = 64;
const PLAYBACK_QUEUE_SIZE: usize = 32;

const FILTERING_WORKERS: usize = 1;
const TRANSFORMATION_WORKERS: usize = 2;
const SYNTHESIS_WORKERS: usize = 4;
const PLAYBACK_WORKERS: usize = 1;

const OPENAI_SECRET_KEY: &str = "secret_openai_api_key";
const OPENAI_BACKEND: &str = "openai";
const PIPELINE_RUNTIME_STATE_KEY: &str = "pipeline_runtime_state";
const DEFAULT_MAX_PREDICTED_QUEUE_DURATION_SECS: u64 = 600;
const ESTIMATED_DURATION_MS_PER_CHAR: u32 = 55;
const MIN_ESTIMATED_DURATION_MS: u32 = 350;
const MAX_ESTIMATED_DURATION_MS: u32 = 30_000;

#[derive(Clone, Debug)]
pub enum PipelineEvent {
    SnapshotUpdated(MessageSnapshot),
    Control(PipelineControlEvent),
    PlaybackStateChanged(PipelinePlaybackState),
    Cleared,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PipelinePlaybackState {
    pub enabled: bool,
    pub active_message_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PipelineControlEvent {
    ChatCommand(ChatCommandEvent),
}

impl PipelineControlEvent {
    fn chat_command(&self) -> Option<&ChatCommandEvent> {
        match self {
            PipelineControlEvent::ChatCommand(command) => Some(command),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatCommandEvent {
    pub message_id: u64,
    pub provider: String,
    pub provider_message_id: Option<String>,
    pub issuer_display_name: String,
    pub issuer_user_id: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub raw: String,
}

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("storage error: {0}")]
    Storage(#[from] lupo_persistence::Error),
    #[error("secret store error: {0}")]
    Secrets(#[from] lupo_secrets::SecureStoreError),
    #[error("tts error: {0}")]
    Tts(#[from] lupo_tts_openai::Error),
    #[error("audio error: {0}")]
    Audio(#[from] lupo_audio::Error),
    #[error("pipeline command channel closed")]
    ChannelClosed,
}

#[derive(Clone)]
pub struct MessagePipeline {
    shared: Arc<PipelineShared>,
    command_tx: mpsc::Sender<PipelineCommand>,
    event_tx: broadcast::Sender<PipelineEvent>,
}

struct PipelineShared {
    runtime: Arc<Runtime>,
    db: lupo_persistence::DatabaseHandle,
    audio: Arc<Audio>,
    settings: SettingsService,
    state: RwLock<HashMap<u64, MessageSnapshot>>,
}

enum PipelineCommand {
    Register(MessageSnapshot),
    EnsureScheduled(u64),
    ModeratorDelete {
        provider: String,
        provider_message_id: String,
    },
    StageResult(StageResult),
    Control {
        action: ControlAction,
        source: CommandSource,
        respond_to: Option<oneshot::Sender<Result<(), PipelineError>>>,
    },
    ClearAll {
        respond_to: oneshot::Sender<Result<(), PipelineError>>,
    },
}

enum StageResult {
    FilteringPassed {
        message_id: u64,
        worker_id: String,
    },
    FilteringSkipped {
        message_id: u64,
        worker_id: String,
        reason: SkipReason,
        note: Option<String>,
        control_event: Option<PipelineControlEvent>,
    },
    TransformationOk {
        message_id: u64,
        worker_id: String,
        text: String,
    },
    SynthesisOk {
        message_id: u64,
        worker_id: String,
        attempt: u32,
        artifact_key: String,
        audio_bytes: Vec<u8>,
        duration_ms: u32,
        sha256: String,
    },
    SynthesisErr {
        message_id: u64,
        worker_id: String,
        attempt: u32,
        error: StageError,
    },
    PlaybackStarted {
        message_id: u64,
        worker_id: String,
        playback_token: u64,
    },
    PlaybackOk {
        message_id: u64,
        worker_id: String,
        duration_ms: u32,
        playback_token: u64,
    },
    PlaybackErr {
        message_id: u64,
        worker_id: String,
        error: StageError,
        playback_token: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandSource {
    Operator,
    Moderator,
}

#[derive(Clone, Debug)]
enum ControlAction {
    Skip { message_id: u64 },
    Retry { message_id: u64 },
    Unskip { message_id: u64 },
    PinNext { message_id: u64 },
    ForcePlay { message_id: u64 },
    SetPlaybackEnabled { enabled: bool },
    StopNow,
}

#[derive(Clone, Debug)]
struct StageError {
    code: String,
    detail: Option<String>,
    retryable: bool,
}

impl StageError {
    fn non_retryable(code: impl Into<String>, detail: impl Into<Option<String>>) -> Self {
        Self {
            code: code.into(),
            detail: detail.into(),
            retryable: false,
        }
    }

    fn retryable(code: impl Into<String>, detail: impl Into<Option<String>>) -> Self {
        Self {
            code: code.into(),
            detail: detail.into(),
            retryable: true,
        }
    }
}

#[derive(Debug)]
struct FilteringJob {
    message_id: u64,
    header: MessageHeader,
}

#[derive(Debug)]
struct TransformationJob {
    message_id: u64,
    raw_content: String,
}

#[derive(Debug)]
struct SynthesisJob {
    message_id: u64,
    text: String,
    attempt: u32,
}

#[derive(Debug)]
struct PlaybackJob {
    message_id: u64,
    artifact_key: String,
    duration_ms: u32,
    playback_token: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedPipelineRuntimeState {
    playback_enabled: bool,
    queue_cursor: Option<u64>,
    pinned_next: Option<u64>,
    forced_queue: Vec<u64>,
}

impl Default for PersistedPipelineRuntimeState {
    fn default() -> Self {
        Self {
            playback_enabled: true,
            queue_cursor: None,
            pinned_next: None,
            forced_queue: Vec::new(),
        }
    }
}

struct PipelineStages {
    filtering: StageHandle<FilteringJob>,
    transformation: StageHandle<TransformationJob>,
    synthesis: StageHandle<SynthesisJob>,
    playback: StageHandle<PlaybackJob>,
}

struct StageHandle<T> {
    sender: mpsc::Sender<T>,
}

impl<T> StageHandle<T>
where
    T: Send + 'static,
{
    fn try_dispatch(&self, job: T) -> Result<(), mpsc::error::TrySendError<T>> {
        self.sender.try_send(job)
    }
}

impl MessagePipeline {
    pub fn start(
        runtime: Arc<Runtime>,
        db: lupo_persistence::DatabaseHandle,
        audio: Arc<Audio>,
        settings: SettingsService,
    ) -> Arc<Self> {
        let (command_tx, command_rx) = mpsc::channel(512);
        let (event_tx, _) = broadcast::channel(512);

        let shared = Arc::new(PipelineShared {
            runtime: runtime.clone(),
            db: db.clone(),
            audio: audio.clone(),
            settings: settings.clone(),
            state: RwLock::new(HashMap::new()),
        });

        PipelineCore::spawn(
            shared.clone(),
            event_tx.clone(),
            command_rx,
            command_tx.clone(),
        );

        let pipeline = Arc::new(Self {
            shared,
            command_tx,
            event_tx,
        });

        pipeline.bootstrap_existing();

        pipeline
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PipelineEvent> {
        self.event_tx.subscribe()
    }

    pub async fn submit_manual_message(
        &self,
        sender: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<MessageSnapshot, PipelineError> {
        let request = NewMessage::new("manual", sender, content, OffsetDateTime::now_utc());
        self.submit(request).await
    }

    pub async fn submit(&self, request: NewMessage) -> Result<MessageSnapshot, PipelineError> {
        let snapshot = enqueue_message(&self.shared.db, request)?;
        self.command_tx
            .send(PipelineCommand::Register(snapshot.clone()))
            .await
            .map_err(|_| PipelineError::ChannelClosed)?;
        Ok(snapshot)
    }

    pub async fn mark_moderator_deleted(
        &self,
        provider: impl Into<String>,
        provider_message_id: impl Into<String>,
    ) -> Result<(), PipelineError> {
        self.command_tx
            .send(PipelineCommand::ModeratorDelete {
                provider: provider.into(),
                provider_message_id: provider_message_id.into(),
            })
            .await
            .map_err(|_| PipelineError::ChannelClosed)
    }

    pub async fn ensure_all(&self) -> Result<(), PipelineError> {
        let ids: Vec<u64> = {
            let state = self.shared.state.read().await;
            state.keys().copied().collect()
        };

        for id in ids {
            self.command_tx
                .send(PipelineCommand::EnsureScheduled(id))
                .await
                .map_err(|_| PipelineError::ChannelClosed)?;
        }

        Ok(())
    }

    pub async fn clear_all(&self) -> Result<(), PipelineError> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(PipelineCommand::ClearAll { respond_to: tx })
            .await
            .map_err(|_| PipelineError::ChannelClosed)?;
        rx.await.map_err(|_| PipelineError::ChannelClosed)??;
        Ok(())
    }

    pub async fn set_playback_enabled(&self, enabled: bool) -> Result<(), PipelineError> {
        self.send_control_action(
            ControlAction::SetPlaybackEnabled { enabled },
            CommandSource::Operator,
        )
        .await
    }

    pub async fn stop_now(&self) -> Result<(), PipelineError> {
        self.send_control_action(ControlAction::StopNow, CommandSource::Operator)
            .await
    }

    #[allow(dead_code)]
    pub async fn skip_message(&self, message_id: u64) -> Result<(), PipelineError> {
        self.send_control_action(ControlAction::Skip { message_id }, CommandSource::Operator)
            .await
    }

    #[allow(dead_code)]
    pub async fn retry_message(&self, message_id: u64) -> Result<(), PipelineError> {
        self.send_control_action(ControlAction::Retry { message_id }, CommandSource::Operator)
            .await
    }

    #[allow(dead_code)]
    pub async fn unskip_message(&self, message_id: u64) -> Result<(), PipelineError> {
        self.send_control_action(
            ControlAction::Unskip { message_id },
            CommandSource::Operator,
        )
        .await
    }

    #[allow(dead_code)]
    pub async fn pin_next(&self, message_id: u64) -> Result<(), PipelineError> {
        self.send_control_action(
            ControlAction::PinNext { message_id },
            CommandSource::Operator,
        )
        .await
    }

    #[allow(dead_code)]
    pub async fn force_play(&self, message_id: u64) -> Result<(), PipelineError> {
        self.send_control_action(
            ControlAction::ForcePlay { message_id },
            CommandSource::Operator,
        )
        .await
    }

    #[allow(dead_code)]
    pub async fn snapshots(&self) -> Vec<MessageSnapshot> {
        let state = self.shared.state.read().await;
        let mut list: Vec<_> = state.values().cloned().collect();
        list.sort_by(|a, b| b.header.id.cmp(&a.header.id));
        list
    }

    pub fn snapshots_blocking(&self) -> Vec<MessageSnapshot> {
        let shared = self.shared.clone();
        let runtime = shared.runtime.clone();
        runtime.block_on(async move { MessagePipeline::snapshots_inner(&shared).await })
    }

    pub fn playback_state_blocking(&self) -> PipelinePlaybackState {
        let enabled = load_runtime_state(&self.shared.db)
            .map(|state| state.playback_enabled)
            .unwrap_or(true);
        let active_message_id = self.snapshots_blocking().into_iter().find_map(|snapshot| {
            matches!(snapshot.state_kind(), MessageStateKind::Playing).then_some(snapshot.header.id)
        });
        PipelinePlaybackState {
            enabled,
            active_message_id,
        }
    }

    async fn snapshots_inner(shared: &PipelineShared) -> Vec<MessageSnapshot> {
        let state = shared.state.read().await;
        let mut list: Vec<_> = state.values().cloned().collect();
        list.sort_by(|a, b| b.header.id.cmp(&a.header.id));
        list
    }

    async fn send_control_action(
        &self,
        action: ControlAction,
        source: CommandSource,
    ) -> Result<(), PipelineError> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(PipelineCommand::Control {
                action,
                source,
                respond_to: Some(tx),
            })
            .await
            .map_err(|_| PipelineError::ChannelClosed)?;
        rx.await.map_err(|_| PipelineError::ChannelClosed)??;
        Ok(())
    }

    fn bootstrap_existing(&self) {
        let snapshots = match list_messages_desc(&self.shared.db) {
            Ok(existing) => existing,
            Err(err) => {
                error!("failed to list existing messages: {err}");
                Vec::new()
            }
        };

        {
            let shared = self.shared.clone();
            let runtime = shared.runtime.clone();
            let seed = snapshots.clone();
            runtime.block_on(async move {
                let mut state = shared.state.write().await;
                state.clear();
                for snapshot in &seed {
                    state.insert(snapshot.header.id, snapshot.clone());
                }
            });
        }

        for snapshot in snapshots {
            if let Err(err) = self
                .command_tx
                .blocking_send(PipelineCommand::Register(snapshot))
            {
                error!("failed to seed pipeline state: {err}");
                break;
            }
        }
    }
}

struct PipelineCore {
    shared: Arc<PipelineShared>,
    event_tx: broadcast::Sender<PipelineEvent>,
    command_tx: mpsc::Sender<PipelineCommand>,
    command_rx: mpsc::Receiver<PipelineCommand>,
    stages: PipelineStages,
    playback_enabled: bool,
    active_playback: Option<u64>,
    playback_token: u64,
    predicted_duration_ms: HashMap<u64, u32>,
    forced_queue: VecDeque<u64>,
    forced_set: HashSet<u64>,
    pinned_next: Option<u64>,
    queue_cursor: Option<u64>,
}

impl PipelineCore {
    fn spawn(
        shared: Arc<PipelineShared>,
        event_tx: broadcast::Sender<PipelineEvent>,
        command_rx: mpsc::Receiver<PipelineCommand>,
        command_tx: mpsc::Sender<PipelineCommand>,
    ) {
        let runtime = shared.runtime.clone();
        runtime.spawn(async move {
            let stages = PipelineStages::new(shared.clone(), command_tx.clone());
            let persisted = load_runtime_state(&shared.db).unwrap_or_default();
            let mut forced_queue = VecDeque::new();
            let mut forced_set = HashSet::new();
            for message_id in persisted.forced_queue {
                if forced_set.insert(message_id) {
                    forced_queue.push_back(message_id);
                }
            }
            let mut core = PipelineCore {
                shared,
                event_tx,
                command_tx: command_tx.clone(),
                command_rx,
                stages,
                playback_enabled: persisted.playback_enabled,
                active_playback: None,
                playback_token: 0,
                predicted_duration_ms: HashMap::new(),
                forced_queue,
                forced_set,
                pinned_next: persisted.pinned_next,
                queue_cursor: persisted.queue_cursor,
            };
            core.emit_playback_state();
            core.run().await;
        });
    }

    async fn run(&mut self) {
        while let Some(command) = self.command_rx.recv().await {
            match command {
                PipelineCommand::Register(snapshot) => {
                    self.handle_register(snapshot).await;
                }
                PipelineCommand::EnsureScheduled(message_id) => {
                    self.ensure_scheduled(message_id).await;
                }
                PipelineCommand::ModeratorDelete {
                    provider,
                    provider_message_id,
                } => {
                    self.handle_moderator_delete(provider, provider_message_id)
                        .await;
                }
                PipelineCommand::StageResult(result) => {
                    self.handle_stage_result(result).await;
                }
                PipelineCommand::Control {
                    action,
                    source,
                    respond_to,
                } => {
                    let result = self.handle_control_action(action, source).await;
                    if let Some(respond_to) = respond_to {
                        let _ = respond_to.send(result);
                    }
                }
                PipelineCommand::ClearAll { respond_to } => {
                    let result = self.handle_clear_all().await;
                    let _ = respond_to.send(result);
                }
            }
        }
    }

    async fn handle_register(&mut self, snapshot: MessageSnapshot) {
        let message_id = snapshot.header.id;
        {
            let mut state = self.shared.state.write().await;
            state.insert(message_id, snapshot.clone());
        }

        self.persist_and_emit(snapshot.clone());

        if let Err(err) = self
            .command_tx
            .send(PipelineCommand::EnsureScheduled(message_id))
            .await
        {
            error!("failed to schedule message {message_id}: {err}");
        }
    }

    async fn ensure_scheduled(&mut self, message_id: u64) {
        let state_kind = {
            let state = self.shared.state.read().await;
            state.get(&message_id).map(|snapshot| snapshot.state_kind())
        };

        match state_kind {
            Some(MessageStateKind::Arrived) => {
                self.schedule_filtering(message_id).await;
            }
            Some(MessageStateKind::Filtered) | Some(MessageStateKind::Transforming) => {
                self.schedule_transformation(message_id).await;
            }
            Some(MessageStateKind::Transformed) | Some(MessageStateKind::Synthesizing) => {
                self.schedule_synthesis(message_id, 1, None).await;
            }
            Some(MessageStateKind::Synthesized) | Some(MessageStateKind::Queued) => {
                self.schedule_playback(message_id, None).await;
            }
            Some(MessageStateKind::Playing) => {
                self.requeue_playing_message(message_id).await;
                self.dispatch_next_playback().await;
            }
            _ => {}
        }
    }

    async fn handle_stage_result(&mut self, result: StageResult) {
        match result {
            StageResult::FilteringPassed {
                message_id,
                worker_id,
            } => {
                self.on_filtering_passed(message_id, worker_id).await;
            }
            StageResult::FilteringSkipped {
                message_id,
                worker_id,
                reason,
                note,
                control_event,
            } => {
                self.on_filtering_skipped(message_id, worker_id, reason, note, control_event)
                    .await;
            }
            StageResult::TransformationOk {
                message_id,
                worker_id,
                text,
            } => {
                self.on_transformation_ok(message_id, worker_id, text).await;
            }
            StageResult::SynthesisOk {
                message_id,
                worker_id,
                attempt,
                artifact_key,
                audio_bytes,
                duration_ms,
                sha256,
            } => {
                self.on_synthesis_ok(
                    message_id,
                    worker_id,
                    attempt,
                    artifact_key,
                    audio_bytes,
                    duration_ms,
                    sha256,
                )
                .await;
            }
            StageResult::SynthesisErr {
                message_id,
                worker_id,
                attempt,
                error,
            } => {
                debug!(
                    "synthesis failed for {message_id} (attempt {attempt}): {}",
                    error.code
                );
                self.on_stage_error(message_id, PipelineStage::Synthesis, worker_id, error)
                    .await;
            }
            StageResult::PlaybackStarted {
                message_id,
                worker_id,
                playback_token,
            } => {
                self.on_playback_started(message_id, worker_id, playback_token)
                    .await;
            }
            StageResult::PlaybackOk {
                message_id,
                worker_id,
                duration_ms,
                playback_token,
            } => {
                self.on_playback_ok(message_id, worker_id, duration_ms, playback_token)
                    .await;
            }
            StageResult::PlaybackErr {
                message_id,
                worker_id,
                error,
                playback_token,
            } => {
                if playback_token == self.playback_token {
                    self.on_stage_error(message_id, PipelineStage::Playback, worker_id, error)
                        .await;
                    if self.active_playback == Some(message_id) {
                        self.active_playback = None;
                        self.emit_playback_state();
                    }
                    self.dispatch_next_playback().await;
                }
            }
        }
    }

    async fn handle_clear_all(&mut self) -> Result<(), PipelineError> {
        clear_all_messages(&self.shared.db)?;
        {
            let mut state = self.shared.state.write().await;
            state.clear();
        }
        self.predicted_duration_ms.clear();
        self.forced_queue.clear();
        self.forced_set.clear();
        self.pinned_next = None;
        self.queue_cursor = None;
        self.active_playback = None;
        self.playback_token = self.playback_token.wrapping_add(1);
        self.persist_runtime_state();
        self.emit_playback_state();
        let _ = self.event_tx.send(PipelineEvent::Cleared);
        Ok(())
    }

    async fn handle_control_action(
        &mut self,
        action: ControlAction,
        source: CommandSource,
    ) -> Result<(), PipelineError> {
        match action {
            ControlAction::SetPlaybackEnabled { enabled } => {
                self.playback_enabled = enabled;
                self.persist_runtime_state();
                self.emit_playback_state();
                if enabled {
                    self.dispatch_next_playback().await;
                }
                Ok(())
            }
            ControlAction::StopNow => {
                self.playback_enabled = false;
                self.playback_token = self.playback_token.wrapping_add(1);
                if let Some(message_id) = self.active_playback.take() {
                    self.requeue_playing_message(message_id).await;
                }
                self.shared.audio.stop().await?;
                self.persist_runtime_state();
                self.emit_playback_state();
                Ok(())
            }
            ControlAction::Skip { message_id } => {
                self.apply_skip(message_id, source).await?;
                Ok(())
            }
            ControlAction::Retry { message_id } => {
                self.apply_retry(message_id, source).await?;
                Ok(())
            }
            ControlAction::Unskip { message_id } => {
                self.apply_unskip(message_id, source).await?;
                Ok(())
            }
            ControlAction::PinNext { message_id } => {
                self.apply_pin_next(message_id, source).await?;
                Ok(())
            }
            ControlAction::ForcePlay { message_id } => {
                self.apply_force_play(message_id, source).await?;
                Ok(())
            }
        }
    }

    async fn requeue_playing_message(&mut self, message_id: u64) {
        let updated_snapshot = {
            let mut state = self.shared.state.write().await;
            let Some(snapshot) = state.get_mut(&message_id) else {
                return;
            };

            if !matches!(snapshot.state_kind(), MessageStateKind::Playing) {
                return;
            }

            snapshot.lifecycle.transition(
                MessageState::Queued(QueuedState {
                    queued_at: OffsetDateTime::now_utc(),
                    priority: QueuePriority::Urgent,
                }),
                TransitionCause::ManualOverride,
                Some("operator".to_string()),
                Some("stop_now_requeue".to_string()),
            );
            Some(snapshot.clone())
        };

        if let Some(snapshot) = updated_snapshot {
            self.persist_and_emit(snapshot);
        }
    }

    async fn apply_skip(
        &mut self,
        message_id: u64,
        source: CommandSource,
    ) -> Result<(), PipelineError> {
        let should_ignore = {
            let state = self.shared.state.read().await;
            state
                .get(&message_id)
                .is_some_and(|snapshot| should_ignore_moderator_action(snapshot, source))
        };
        if should_ignore {
            return Ok(());
        }

        let was_active = self.active_playback == Some(message_id);
        if was_active {
            self.playback_token = self.playback_token.wrapping_add(1);
            self.active_playback = None;
            self.shared.audio.stop().await?;
            self.emit_playback_state();
        }

        let updated_snapshot = {
            let mut state = self.shared.state.write().await;
            let Some(snapshot) = state.get_mut(&message_id) else {
                return Ok(());
            };

            if matches!(snapshot.state_kind(), MessageStateKind::Played) {
                return Ok(());
            }

            mark_override_metadata(snapshot, source, true);
            snapshot.lifecycle.transition(
                MessageState::Skipped(SkippedState {
                    at: OffsetDateTime::now_utc(),
                    reason: SkipReason::OperatorOverride,
                    note: Some(match source {
                        CommandSource::Operator => "operator_skip".to_string(),
                        CommandSource::Moderator => "moderator_skip".to_string(),
                    }),
                }),
                match source {
                    CommandSource::Operator => TransitionCause::ManualOverride,
                    CommandSource::Moderator => TransitionCause::Automatic,
                },
                Some(match source {
                    CommandSource::Operator => "operator".to_string(),
                    CommandSource::Moderator => "moderator".to_string(),
                }),
                Some("skip_command".to_string()),
            );
            Some(snapshot.clone())
        };

        self.clear_runtime_hints_for(message_id);
        if let Some(snapshot) = updated_snapshot {
            self.persist_and_emit(snapshot);
        }
        self.dispatch_next_playback().await;
        Ok(())
    }

    async fn apply_retry(
        &mut self,
        message_id: u64,
        source: CommandSource,
    ) -> Result<(), PipelineError> {
        let should_ignore = {
            let state = self.shared.state.read().await;
            state
                .get(&message_id)
                .is_some_and(|snapshot| should_ignore_moderator_action(snapshot, source))
        };
        if should_ignore {
            return Ok(());
        }

        let updated_snapshot = {
            let mut state = self.shared.state.write().await;
            let Some(snapshot) = state.get_mut(&message_id) else {
                return Ok(());
            };

            let next_state = match &snapshot.lifecycle.state {
                MessageState::Error(error) => retry_state_from_error(error, snapshot),
                MessageState::Skipped(_) => unskip_target_state(snapshot),
                MessageState::Played(_) => replay_target_state(snapshot),
                _ => None,
            };

            let Some(next_state) = next_state else {
                return Ok(());
            };

            mark_override_metadata(snapshot, source, false);
            snapshot.lifecycle.transition(
                next_state,
                TransitionCause::Retry,
                Some(match source {
                    CommandSource::Operator => "operator".to_string(),
                    CommandSource::Moderator => "moderator".to_string(),
                }),
                Some("retry_command".to_string()),
            );

            Some(snapshot.clone())
        };

        self.clear_runtime_hints_for(message_id);
        if let Some(snapshot) = updated_snapshot {
            self.persist_and_emit(snapshot);
            self.ensure_scheduled(message_id).await;
        }
        Ok(())
    }

    async fn apply_unskip(
        &mut self,
        message_id: u64,
        source: CommandSource,
    ) -> Result<(), PipelineError> {
        let should_ignore = {
            let state = self.shared.state.read().await;
            state
                .get(&message_id)
                .is_some_and(|snapshot| should_ignore_moderator_action(snapshot, source))
        };
        if should_ignore {
            return Ok(());
        }

        let updated_snapshot = {
            let mut state = self.shared.state.write().await;
            let Some(snapshot) = state.get_mut(&message_id) else {
                return Ok(());
            };

            if !matches!(snapshot.state_kind(), MessageStateKind::Skipped) {
                return Ok(());
            }

            let Some(next_state) = unskip_target_state(snapshot) else {
                return Ok(());
            };

            mark_override_metadata(snapshot, source, false);
            snapshot.lifecycle.transition(
                next_state,
                TransitionCause::ManualOverride,
                Some(match source {
                    CommandSource::Operator => "operator".to_string(),
                    CommandSource::Moderator => "moderator".to_string(),
                }),
                Some("unskip_command".to_string()),
            );

            Some(snapshot.clone())
        };

        self.clear_runtime_hints_for(message_id);
        if let Some(snapshot) = updated_snapshot {
            self.persist_and_emit(snapshot);
            self.ensure_scheduled(message_id).await;
        }
        Ok(())
    }

    async fn apply_pin_next(
        &mut self,
        message_id: u64,
        source: CommandSource,
    ) -> Result<(), PipelineError> {
        let should_ignore = {
            let state = self.shared.state.read().await;
            state
                .get(&message_id)
                .is_some_and(|snapshot| should_ignore_moderator_action(snapshot, source))
        };
        if should_ignore {
            return Ok(());
        }

        let updated_snapshot = {
            let mut state = self.shared.state.write().await;
            let Some(snapshot) = state.get_mut(&message_id) else {
                return Ok(());
            };

            mark_override_metadata(snapshot, source, false);
            snapshot
                .header
                .metadata
                .insert("operator_pin_next".to_string(), Value::Bool(true));
            if matches!(snapshot.state_kind(), MessageStateKind::Synthesized) {
                snapshot.lifecycle.transition(
                    MessageState::Queued(QueuedState {
                        queued_at: OffsetDateTime::now_utc(),
                        priority: QueuePriority::High,
                    }),
                    TransitionCause::ManualOverride,
                    Some("operator".to_string()),
                    Some("pin_next".to_string()),
                );
            } else if let MessageState::Queued(queued) = &mut snapshot.lifecycle.state {
                queued.priority = QueuePriority::High;
            }

            Some(snapshot.clone())
        };

        self.pinned_next = Some(message_id);
        self.persist_runtime_state();
        if let Some(snapshot) = updated_snapshot {
            self.persist_and_emit(snapshot);
        }
        self.dispatch_next_playback().await;
        Ok(())
    }

    async fn apply_force_play(
        &mut self,
        message_id: u64,
        source: CommandSource,
    ) -> Result<(), PipelineError> {
        let should_ignore = {
            let state = self.shared.state.read().await;
            state
                .get(&message_id)
                .is_some_and(|snapshot| should_ignore_moderator_action(snapshot, source))
        };
        if should_ignore {
            return Ok(());
        }

        let updated_snapshot = {
            let mut state = self.shared.state.write().await;
            let Some(snapshot) = state.get_mut(&message_id) else {
                return Ok(());
            };

            mark_override_metadata(snapshot, source, false);
            snapshot
                .header
                .metadata
                .insert("operator_force_include".to_string(), Value::Bool(true));

            if matches!(snapshot.state_kind(), MessageStateKind::Skipped) {
                if let Some(next_state) = unskip_target_state(snapshot) {
                    snapshot.lifecycle.transition(
                        next_state,
                        TransitionCause::ManualOverride,
                        Some("operator".to_string()),
                        Some("force_play_unskip".to_string()),
                    );
                }
            }

            if matches!(snapshot.state_kind(), MessageStateKind::Synthesized) {
                snapshot.lifecycle.transition(
                    MessageState::Queued(QueuedState {
                        queued_at: OffsetDateTime::now_utc(),
                        priority: QueuePriority::Urgent,
                    }),
                    TransitionCause::ManualOverride,
                    Some("operator".to_string()),
                    Some("force_play_queue".to_string()),
                );
            } else if let MessageState::Queued(queued) = &mut snapshot.lifecycle.state {
                queued.priority = QueuePriority::Urgent;
            }

            Some(snapshot.clone())
        };

        if self.forced_set.insert(message_id) {
            self.forced_queue.push_front(message_id);
        }
        self.persist_runtime_state();

        if let Some(snapshot) = updated_snapshot {
            self.persist_and_emit(snapshot);
            self.ensure_scheduled(message_id).await;
        }
        self.dispatch_next_playback().await;
        Ok(())
    }

    async fn handle_moderator_delete(&mut self, provider: String, provider_message_id: String) {
        let updated_snapshot = {
            let mut state = self.shared.state.write().await;
            let message_id = state.iter().find_map(|(id, snapshot)| {
                let same_provider = snapshot.header.provider == provider;
                let same_message_id = snapshot
                    .header
                    .provider_message_id
                    .as_ref()
                    .is_some_and(|candidate| candidate == &provider_message_id);
                if same_provider && same_message_id {
                    Some(*id)
                } else {
                    None
                }
            });

            let Some(message_id) = message_id else {
                return;
            };

            let Some(snapshot) = state.get_mut(&message_id) else {
                return;
            };

            if is_operator_force_include(snapshot)
                || matches!(snapshot.state_kind(), MessageStateKind::Played)
            {
                return;
            }

            if matches!(snapshot.state_kind(), MessageStateKind::Skipped) {
                return;
            }

            snapshot.lifecycle.transition(
                MessageState::Skipped(SkippedState {
                    at: OffsetDateTime::now_utc(),
                    reason: SkipReason::ModeratorDeleted,
                    note: Some("moderator_delete_event".to_string()),
                }),
                TransitionCause::Automatic,
                None,
                Some("moderator_deleted".to_string()),
            );

            Some((snapshot.clone(), self.active_playback == Some(message_id)))
        };

        if let Some((snapshot, was_active)) = updated_snapshot {
            if was_active {
                self.playback_token = self.playback_token.wrapping_add(1);
                self.active_playback = None;
                if let Err(err) = self.shared.audio.stop().await {
                    warn!("failed to stop deleted active message playback: {err}");
                }
                self.emit_playback_state();
            }
            self.clear_runtime_hints_for(snapshot.header.id);
            self.persist_and_emit(snapshot);
            self.dispatch_next_playback().await;
        }
    }

    async fn on_filtering_passed(&mut self, message_id: u64, worker_id: String) {
        let snapshot = {
            let mut state = self.shared.state.write().await;
            let Some(snapshot) = state.get_mut(&message_id) else {
                warn!("filtering completed for unknown message {message_id}");
                return;
            };

            if !matches!(snapshot.state_kind(), MessageStateKind::Arrived) {
                return;
            }

            snapshot
                .lifecycle
                .stage_spans
                .set_worker(PipelineStage::Filtering, Some(worker_id));
            snapshot
                .lifecycle
                .stage_spans
                .finish(PipelineStage::Filtering);
            snapshot.lifecycle.transition(
                MessageState::Filtered,
                TransitionCause::Automatic,
                None,
                Some("filter_pass".into()),
            );
            snapshot.clone()
        };

        self.persist_and_emit(snapshot);
        self.schedule_transformation(message_id).await;
    }

    async fn on_filtering_skipped(
        &mut self,
        message_id: u64,
        worker_id: String,
        reason: SkipReason,
        note: Option<String>,
        control_event: Option<PipelineControlEvent>,
    ) {
        let snapshot = {
            let mut state = self.shared.state.write().await;
            let Some(snapshot) = state.get_mut(&message_id) else {
                warn!("filtering skip reported for unknown message {message_id}");
                return;
            };

            if !matches!(snapshot.state_kind(), MessageStateKind::Arrived) {
                return;
            }

            snapshot
                .lifecycle
                .stage_spans
                .set_worker(PipelineStage::Filtering, Some(worker_id));
            snapshot
                .lifecycle
                .stage_spans
                .finish(PipelineStage::Filtering);
            snapshot.lifecycle.transition(
                MessageState::Skipped(SkippedState {
                    at: OffsetDateTime::now_utc(),
                    reason,
                    note,
                }),
                TransitionCause::Automatic,
                None,
                Some("filter_skip".into()),
            );

            snapshot.clone()
        };

        self.clear_runtime_hints_for(message_id);
        self.persist_and_emit(snapshot);
        if let Some(control_event) = control_event {
            let action = control_event
                .chat_command()
                .and_then(chat_command_to_action);
            let _ = self.event_tx.send(PipelineEvent::Control(control_event));
            if let Some(action) = action {
                if let Err(err) = self
                    .handle_control_action(action, CommandSource::Moderator)
                    .await
                {
                    warn!("failed to apply moderator command action: {err}");
                }
            }
        }
    }

    async fn on_transformation_ok(&mut self, message_id: u64, worker_id: String, text: String) {
        let snapshot = {
            let mut state = self.shared.state.write().await;
            let Some(snapshot) = state.get_mut(&message_id) else {
                warn!("transformation completed for unknown message {message_id}");
                return;
            };

            snapshot
                .lifecycle
                .stage_spans
                .set_worker(PipelineStage::Transformation, Some(worker_id));
            snapshot
                .lifecycle
                .stage_spans
                .finish(PipelineStage::Transformation);
            snapshot.lifecycle.transition(
                MessageState::Transformed(TransformedState {
                    completed_at: OffsetDateTime::now_utc(),
                    text: text.clone(),
                }),
                TransitionCause::Automatic,
                None,
                Some("transform_ok".into()),
            );
            snapshot.clone()
        };

        self.persist_and_emit(snapshot);
        self.schedule_synthesis(message_id, 1, Some(text)).await;
    }

    async fn on_synthesis_ok(
        &mut self,
        message_id: u64,
        worker_id: String,
        attempt: u32,
        artifact_key: String,
        audio_bytes: Vec<u8>,
        duration_ms: u32,
        sha256: String,
    ) {
        let backend = SynthBackendId::from(OPENAI_BACKEND);

        if let Err(err) = store_audio_artifact(
            &self.shared.db,
            message_id,
            &artifact_key,
            audio_bytes.as_slice(),
        ) {
            error!("failed to store audio artifact for {message_id}: {err}");
            let stage_error =
                StageError::non_retryable("artifact_store_failed", Some(err.to_string()));
            self.on_stage_error(message_id, PipelineStage::Synthesis, worker_id, stage_error)
                .await;
            return;
        }

        let snapshot = {
            let mut state = self.shared.state.write().await;
            let Some(snapshot) = state.get_mut(&message_id) else {
                warn!("synthesis completed for unknown message {message_id}");
                return;
            };

            snapshot
                .lifecycle
                .stage_spans
                .set_worker(PipelineStage::Synthesis, Some(worker_id));
            snapshot
                .lifecycle
                .stage_spans
                .finish(PipelineStage::Synthesis);
            snapshot.lifecycle.transition(
                MessageState::Synthesized(SynthesizedState {
                    artifact_key: artifact_key.clone(),
                    backend: backend.clone(),
                    duration_ms,
                    completed_at: OffsetDateTime::now_utc(),
                    sha256: sha256.clone(),
                }),
                TransitionCause::Automatic,
                None,
                Some(format!("synth_ok_attempt_{attempt}")),
            );

            let artifact = Artifact::Audio(AudioArtifact {
                format: AudioFormat::Wav,
                storage_key: artifact_key.clone(),
                duration_ms,
                sha256: sha256.clone(),
                backend: backend.clone(),
            });
            match snapshot.artifacts.get("synth") {
                Some(existing) if matches!(existing, Artifact::Audio(_)) => {
                    snapshot.artifacts.remove("synth");
                }
                _ => {}
            }
            snapshot.artifacts.insert("synth", artifact);

            snapshot.clone()
        };

        self.predicted_duration_ms.remove(&message_id);

        let should_skip_for_overflow = !is_operator_force_include(&snapshot)
            && self.would_exceed_actual_budget(message_id).await;
        if should_skip_for_overflow {
            let overflow_snapshot = {
                let mut state = self.shared.state.write().await;
                let Some(snapshot) = state.get_mut(&message_id) else {
                    return;
                };
                if !matches!(snapshot.state_kind(), MessageStateKind::Synthesized) {
                    return;
                }
                snapshot.lifecycle.transition(
                    MessageState::Skipped(SkippedState {
                        at: OffsetDateTime::now_utc(),
                        reason: SkipReason::QueueOverflow,
                        note: Some(format!("actual_duration_ms:{duration_ms}")),
                    }),
                    TransitionCause::Automatic,
                    None,
                    Some("queue_overflow".into()),
                );
                Some(snapshot.clone())
            };
            if let Some(snapshot) = overflow_snapshot {
                self.persist_and_emit(snapshot);
            }
            return;
        }

        self.persist_and_emit(snapshot);
        self.schedule_playback(message_id, Some((artifact_key, duration_ms)))
            .await;
    }

    async fn on_playback_started(
        &mut self,
        message_id: u64,
        worker_id: String,
        playback_token: u64,
    ) {
        if playback_token != self.playback_token {
            return;
        }
        let snapshot = {
            let mut state = self.shared.state.write().await;
            let Some(snapshot) = state.get_mut(&message_id) else {
                warn!("playback started for unknown message {message_id}");
                return;
            };

            if !matches!(snapshot.state_kind(), MessageStateKind::Queued) {
                return;
            }

            snapshot
                .lifecycle
                .stage_spans
                .set_worker(PipelineStage::Playback, Some(worker_id.clone()));
            snapshot.lifecycle.transition(
                MessageState::Playing(PlayingState {
                    started_at: OffsetDateTime::now_utc(),
                    worker: Some(worker_id),
                }),
                TransitionCause::Automatic,
                None,
                Some("playback_started".into()),
            );

            snapshot.clone()
        };

        self.active_playback = Some(message_id);
        self.queue_cursor = Some(message_id);
        self.persist_runtime_state();
        self.emit_playback_state();
        self.persist_and_emit(snapshot);
    }

    async fn on_playback_ok(
        &mut self,
        message_id: u64,
        worker_id: String,
        duration_ms: u32,
        playback_token: u64,
    ) {
        if playback_token != self.playback_token {
            return;
        }
        let snapshot = {
            let mut state = self.shared.state.write().await;
            let Some(snapshot) = state.get_mut(&message_id) else {
                warn!("playback finished for unknown message {message_id}");
                return;
            };

            if !matches!(snapshot.state_kind(), MessageStateKind::Playing) {
                return;
            }

            snapshot
                .lifecycle
                .stage_spans
                .set_worker(PipelineStage::Playback, Some(worker_id));
            snapshot
                .lifecycle
                .stage_spans
                .finish(PipelineStage::Playback);
            snapshot.lifecycle.transition(
                MessageState::Played(PlayedState {
                    completed_at: OffsetDateTime::now_utc(),
                    duration_ms,
                }),
                TransitionCause::Automatic,
                None,
                Some("playback_complete".into()),
            );

            snapshot.clone()
        };

        self.active_playback = None;
        self.queue_cursor = Some(message_id);
        self.clear_runtime_hints_for(message_id);
        self.persist_runtime_state();
        self.emit_playback_state();
        self.persist_and_emit(snapshot);
        self.dispatch_next_playback().await;
    }

    async fn on_stage_error(
        &mut self,
        message_id: u64,
        stage: PipelineStage,
        worker_id: String,
        error: StageError,
    ) {
        let snapshot = {
            let mut state = self.shared.state.write().await;
            let Some(snapshot) = state.get_mut(&message_id) else {
                warn!("error reported for unknown message {message_id}");
                return;
            };

            snapshot
                .lifecycle
                .stage_spans
                .set_worker(stage, Some(worker_id));
            snapshot.lifecycle.stage_spans.finish(stage);
            let note = if error.retryable {
                "stage_error_retryable"
            } else {
                "stage_error"
            };

            snapshot.lifecycle.transition(
                MessageState::Error(ErrorState {
                    at: OffsetDateTime::now_utc(),
                    stage: Some(stage),
                    code: error.code.clone(),
                    detail: error.detail.clone(),
                }),
                TransitionCause::Automatic,
                None,
                Some(note.into()),
            );

            snapshot.clone()
        };

        self.clear_runtime_hints_for(message_id);
        self.persist_and_emit(snapshot);
    }

    async fn schedule_filtering(&mut self, message_id: u64) {
        let maybe = {
            let mut state = self.shared.state.write().await;
            let Some(snapshot) = state.get_mut(&message_id) else {
                return;
            };

            if !matches!(snapshot.state_kind(), MessageStateKind::Arrived) {
                return;
            }

            snapshot
                .lifecycle
                .stage_spans
                .start(PipelineStage::Filtering, None);
            let job = FilteringJob {
                message_id,
                header: snapshot.header.clone(),
            };

            Some((snapshot.clone(), job))
        };

        if let Some((snapshot, job)) = maybe {
            self.persist_and_emit(snapshot);
            if let Err(err) = self.stages.filtering.try_dispatch(job) {
                let stage_error = match err {
                    mpsc::error::TrySendError::Full(_) => StageError::non_retryable(
                        "stage_queue_full",
                        Some("filtering_queue".to_string()),
                    ),
                    mpsc::error::TrySendError::Closed(_) => StageError::non_retryable(
                        "stage_queue_closed",
                        Some("filtering_queue".to_string()),
                    ),
                };
                self.on_stage_error(
                    message_id,
                    PipelineStage::Filtering,
                    "scheduler".to_string(),
                    stage_error,
                )
                .await;
            }
        }
    }

    async fn schedule_transformation(&mut self, message_id: u64) {
        let maybe = {
            let mut state = self.shared.state.write().await;
            let Some(snapshot) = state.get_mut(&message_id) else {
                return;
            };

            let should_transition = match snapshot.state_kind() {
                MessageStateKind::Filtered => true,
                MessageStateKind::Transforming => false,
                _ => return,
            };

            snapshot
                .lifecycle
                .stage_spans
                .start(PipelineStage::Transformation, None);
            if should_transition {
                snapshot.lifecycle.transition(
                    MessageState::Transforming(TransformingState {
                        started_at: OffsetDateTime::now_utc(),
                        worker: None,
                    }),
                    TransitionCause::Automatic,
                    None,
                    Some("transform_start".into()),
                );
            }

            let job = TransformationJob {
                message_id,
                raw_content: snapshot.header.content.clone(),
            };

            Some((snapshot.clone(), job))
        };

        if let Some((snapshot, job)) = maybe {
            self.persist_and_emit(snapshot);
            if let Err(err) = self.stages.transformation.try_dispatch(job) {
                let stage_error = match err {
                    mpsc::error::TrySendError::Full(_) => StageError::non_retryable(
                        "stage_queue_full",
                        Some("transformation_queue".to_string()),
                    ),
                    mpsc::error::TrySendError::Closed(_) => StageError::non_retryable(
                        "stage_queue_closed",
                        Some("transformation_queue".to_string()),
                    ),
                };
                self.on_stage_error(
                    message_id,
                    PipelineStage::Transformation,
                    "scheduler".to_string(),
                    stage_error,
                )
                .await;
            }
        }
    }

    async fn schedule_synthesis(
        &mut self,
        message_id: u64,
        attempt: u32,
        text_override: Option<String>,
    ) {
        let (text_preview, force_include) = {
            let state = self.shared.state.read().await;
            let Some(snapshot) = state.get(&message_id) else {
                return;
            };

            let text = if let Some(text) = &text_override {
                text.clone()
            } else if let MessageState::Transformed(transformed) = &snapshot.lifecycle.state {
                transformed.text.clone()
            } else {
                snapshot.header.content.clone()
            };
            (text, is_operator_force_include(snapshot))
        };

        let predicted_duration_ms = estimate_duration_ms(&text_preview);
        self.predicted_duration_ms
            .insert(message_id, predicted_duration_ms);

        if !force_include
            && self
                .would_exceed_predicted_budget(message_id, predicted_duration_ms)
                .await
        {
            self.predicted_duration_ms.remove(&message_id);
            let skipped_snapshot = {
                let mut state = self.shared.state.write().await;
                let Some(snapshot) = state.get_mut(&message_id) else {
                    return;
                };

                if !matches!(
                    snapshot.state_kind(),
                    MessageStateKind::Transformed | MessageStateKind::Synthesizing
                ) {
                    return;
                }

                snapshot.lifecycle.transition(
                    MessageState::Skipped(SkippedState {
                        at: OffsetDateTime::now_utc(),
                        reason: SkipReason::PredictedQueueOverflow,
                        note: Some(format!("predicted_duration_ms:{predicted_duration_ms}")),
                    }),
                    TransitionCause::Automatic,
                    None,
                    Some("predicted_queue_overflow".into()),
                );
                Some(snapshot.clone())
            };

            if let Some(snapshot) = skipped_snapshot {
                self.persist_and_emit(snapshot);
            }
            return;
        }

        let maybe = {
            let mut state = self.shared.state.write().await;
            let Some(snapshot) = state.get_mut(&message_id) else {
                return;
            };

            let should_transition = match snapshot.state_kind() {
                MessageStateKind::Transformed => true,
                MessageStateKind::Synthesizing => false,
                _ => return,
            };

            let text = if let Some(text) = text_override {
                text
            } else if let MessageState::Transformed(transformed) = &snapshot.lifecycle.state {
                transformed.text.clone()
            } else {
                snapshot.header.content.clone()
            };

            snapshot
                .lifecycle
                .stage_spans
                .start(PipelineStage::Synthesis, None);
            if should_transition {
                snapshot.lifecycle.transition(
                    MessageState::Synthesizing(SynthesizingState {
                        backend: Some(SynthBackendId::from(OPENAI_BACKEND)),
                        attempt,
                        started_at: OffsetDateTime::now_utc(),
                        worker: None,
                    }),
                    TransitionCause::Automatic,
                    None,
                    Some(format!("synth_start_attempt_{attempt}")),
                );
            }

            let job = SynthesisJob {
                message_id,
                text,
                attempt,
            };

            Some((snapshot.clone(), job))
        };

        if let Some((snapshot, job)) = maybe {
            self.persist_and_emit(snapshot);
            if let Err(err) = self.stages.synthesis.try_dispatch(job) {
                let stage_error = match err {
                    mpsc::error::TrySendError::Full(_) => StageError::non_retryable(
                        "stage_queue_full",
                        Some("synthesis_queue".to_string()),
                    ),
                    mpsc::error::TrySendError::Closed(_) => StageError::non_retryable(
                        "stage_queue_closed",
                        Some("synthesis_queue".to_string()),
                    ),
                };
                self.on_stage_error(
                    message_id,
                    PipelineStage::Synthesis,
                    "scheduler".to_string(),
                    stage_error,
                )
                .await;
            }
        }
    }

    async fn schedule_playback(&mut self, message_id: u64, _artifact: Option<(String, u32)>) {
        let updated_snapshot = {
            let mut state = self.shared.state.write().await;
            let Some(snapshot) = state.get_mut(&message_id) else {
                return;
            };

            if matches!(snapshot.state_kind(), MessageStateKind::Synthesized) {
                snapshot
                    .lifecycle
                    .stage_spans
                    .start(PipelineStage::Playback, None);
                snapshot.lifecycle.transition(
                    MessageState::Queued(QueuedState {
                        queued_at: OffsetDateTime::now_utc(),
                        priority: queue_priority_for_snapshot(snapshot),
                    }),
                    TransitionCause::Automatic,
                    None,
                    Some("playback_queued".into()),
                );
                Some(snapshot.clone())
            } else {
                None
            }
        };

        if let Some(snapshot) = updated_snapshot {
            self.persist_and_emit(snapshot);
        }

        self.dispatch_next_playback().await;
    }

    async fn dispatch_next_playback(&mut self) {
        if !self.playback_enabled || self.active_playback.is_some() {
            return;
        }

        let message_id = {
            let state = self.shared.state.read().await;
            let Some(message_id) = self.next_playback_candidate(&state) else {
                return;
            };
            message_id
        };

        self.consume_playback_hint(message_id);

        let (artifact_key, duration_ms) = {
            let state = self.shared.state.read().await;

            let Some(snapshot) = state.get(&message_id) else {
                return;
            };

            if !matches!(snapshot.state_kind(), MessageStateKind::Queued) {
                return;
            }

            match snapshot.artifacts.get("synth") {
                Some(Artifact::Audio(audio)) => (audio.storage_key.clone(), audio.duration_ms),
                _ => {
                    warn!("no synthesized artifact available for {message_id}");
                    return;
                }
            }
        };

        self.active_playback = Some(message_id);
        self.emit_playback_state();
        self.persist_runtime_state();

        let job = PlaybackJob {
            message_id,
            artifact_key,
            duration_ms,
            playback_token: self.playback_token,
        };

        if let Err(err) = self.stages.playback.try_dispatch(job) {
            self.active_playback = None;
            self.emit_playback_state();

            let stage_error = match err {
                mpsc::error::TrySendError::Full(_) => StageError::non_retryable(
                    "stage_queue_full",
                    Some("playback_queue".to_string()),
                ),
                mpsc::error::TrySendError::Closed(_) => StageError::non_retryable(
                    "stage_queue_closed",
                    Some("playback_queue".to_string()),
                ),
            };
            self.on_stage_error(
                message_id,
                PipelineStage::Playback,
                "scheduler".to_string(),
                stage_error,
            )
            .await;
        }
    }

    fn next_playback_candidate(&self, state: &HashMap<u64, MessageSnapshot>) -> Option<u64> {
        if let Some(pinned_id) = self.pinned_next {
            if state
                .get(&pinned_id)
                .is_some_and(|snapshot| matches!(snapshot.state_kind(), MessageStateKind::Queued))
            {
                return Some(pinned_id);
            }
        }

        for forced_id in self.forced_queue.iter() {
            if state
                .get(forced_id)
                .is_some_and(|snapshot| matches!(snapshot.state_kind(), MessageStateKind::Queued))
            {
                return Some(*forced_id);
            }
        }

        let mut queued = state
            .iter()
            .filter_map(|(id, snapshot)| match &snapshot.lifecycle.state {
                MessageState::Queued(queued) => {
                    Some((*id, queued.priority.clone(), queued.queued_at))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        queued.sort_by(|(a_id, a_priority, a_at), (b_id, b_priority, b_at)| {
            queue_priority_rank(b_priority)
                .cmp(&queue_priority_rank(a_priority))
                .then_with(|| a_at.cmp(b_at))
                .then_with(|| a_id.cmp(b_id))
        });
        queued.first().map(|entry| entry.0)
    }

    fn consume_playback_hint(&mut self, message_id: u64) {
        let mut changed = false;
        if self.pinned_next == Some(message_id) {
            self.pinned_next = None;
            changed = true;
        }
        if self.forced_set.remove(&message_id) {
            self.forced_queue
                .retain(|candidate| *candidate != message_id);
            changed = true;
        } else if self
            .forced_queue
            .iter()
            .any(|candidate| *candidate == message_id)
        {
            self.forced_queue
                .retain(|candidate| *candidate != message_id);
            changed = true;
        }

        if changed {
            self.persist_runtime_state();
        }
    }

    fn emit_playback_state(&self) {
        let _ = self
            .event_tx
            .send(PipelineEvent::PlaybackStateChanged(PipelinePlaybackState {
                enabled: self.playback_enabled,
                active_message_id: self.active_playback,
            }));
    }

    fn persist_runtime_state(&self) {
        let mut forced_queue = Vec::with_capacity(self.forced_queue.len());
        for message_id in &self.forced_queue {
            forced_queue.push(*message_id);
        }
        let state = PersistedPipelineRuntimeState {
            playback_enabled: self.playback_enabled,
            queue_cursor: self.queue_cursor,
            pinned_next: self.pinned_next,
            forced_queue,
        };
        if let Err(err) = save_runtime_state(&self.shared.db, &state) {
            warn!("failed to persist pipeline runtime state: {err}");
        }
    }

    fn clear_runtime_hints_for(&mut self, message_id: u64) {
        self.predicted_duration_ms.remove(&message_id);
        if self.pinned_next == Some(message_id) {
            self.pinned_next = None;
        }
        self.forced_queue
            .retain(|candidate| *candidate != message_id);
        self.forced_set.remove(&message_id);
        self.persist_runtime_state();
    }

    fn max_predicted_queue_duration_ms(&self) -> u64 {
        load_max_predicted_queue_duration_ms(&self.shared.settings)
    }

    async fn would_exceed_predicted_budget(
        &self,
        pending_message_id: u64,
        pending_duration_ms: u32,
    ) -> bool {
        let max_duration_ms = self.max_predicted_queue_duration_ms();
        let state = self.shared.state.read().await;

        let mut total_ms = 0_u64;
        let mut includes_pending = false;
        for (message_id, snapshot) in state.iter() {
            match snapshot.state_kind() {
                MessageStateKind::Queued
                | MessageStateKind::Playing
                | MessageStateKind::Synthesized => {
                    let duration = snapshot_actual_duration_ms(snapshot)
                        .or_else(|| self.predicted_duration_ms.get(message_id).copied())
                        .unwrap_or(0);
                    total_ms = total_ms.saturating_add(duration as u64);
                    if *message_id == pending_message_id {
                        includes_pending = true;
                    }
                }
                MessageStateKind::Transforming
                | MessageStateKind::Transformed
                | MessageStateKind::Synthesizing => {
                    if let Some(duration) = self.predicted_duration_ms.get(message_id).copied() {
                        total_ms = total_ms.saturating_add(duration as u64);
                        if *message_id == pending_message_id {
                            includes_pending = true;
                        }
                    }
                }
                _ => {}
            }
        }

        if !includes_pending {
            total_ms = total_ms.saturating_add(pending_duration_ms as u64);
        }
        total_ms > max_duration_ms
    }

    async fn would_exceed_actual_budget(&self, pending_message_id: u64) -> bool {
        let max_duration_ms = self.max_predicted_queue_duration_ms();
        let state = self.shared.state.read().await;

        let mut total_ms = 0_u64;
        let mut includes_pending = false;
        for (message_id, snapshot) in state.iter() {
            match snapshot.state_kind() {
                MessageStateKind::Queued
                | MessageStateKind::Playing
                | MessageStateKind::Synthesized => {
                    if let Some(duration) = snapshot_actual_duration_ms(snapshot) {
                        total_ms = total_ms.saturating_add(duration as u64);
                        if *message_id == pending_message_id {
                            includes_pending = true;
                        }
                    }
                }
                _ => {}
            }
        }

        if !includes_pending {
            if let Some(snapshot) = state.get(&pending_message_id) {
                if let Some(duration) = snapshot_actual_duration_ms(snapshot) {
                    total_ms = total_ms.saturating_add(duration as u64);
                }
            }
        }
        total_ms > max_duration_ms
    }

    fn persist_and_emit(&self, snapshot: MessageSnapshot) {
        if let Err(err) = save_message_snapshot(&self.shared.db, &snapshot) {
            error!("failed to persist message {}: {err}", snapshot.header.id);
        }
        let _ = self.event_tx.send(PipelineEvent::SnapshotUpdated(snapshot));
    }
}

impl PipelineStages {
    fn new(shared: Arc<PipelineShared>, command_tx: mpsc::Sender<PipelineCommand>) -> Self {
        Self {
            filtering: spawn_filtering_stage(shared.clone(), command_tx.clone()),
            transformation: spawn_transformation_stage(shared.clone(), command_tx.clone()),
            synthesis: spawn_synthesis_stage(shared.clone(), command_tx.clone()),
            playback: spawn_playback_stage(shared, command_tx),
        }
    }
}

fn spawn_filtering_stage(
    shared: Arc<PipelineShared>,
    command_tx: mpsc::Sender<PipelineCommand>,
) -> StageHandle<FilteringJob> {
    let (tx, rx) = mpsc::channel::<FilteringJob>(FILTERING_QUEUE_SIZE);
    let rx = Arc::new(Mutex::new(rx));

    for index in 0..FILTERING_WORKERS {
        let rx = rx.clone();
        let command_tx = command_tx.clone();
        let shared = shared.clone();
        let runtime = shared.runtime.clone();
        runtime.spawn(async move {
            let worker_id = format!("filter-{}", index);
            loop {
                let job = {
                    let mut guard = rx.lock().await;
                    guard.recv().await
                };

                let Some(job) = job else {
                    break;
                };

                let outcome = process_filtering_job(&shared, &worker_id, job);
                if command_tx
                    .send(PipelineCommand::StageResult(outcome))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    StageHandle { sender: tx }
}

fn spawn_transformation_stage(
    shared: Arc<PipelineShared>,
    command_tx: mpsc::Sender<PipelineCommand>,
) -> StageHandle<TransformationJob> {
    let (tx, rx) = mpsc::channel::<TransformationJob>(TRANSFORMATION_QUEUE_SIZE);
    let rx = Arc::new(Mutex::new(rx));

    for index in 0..TRANSFORMATION_WORKERS {
        let rx = rx.clone();
        let command_tx = command_tx.clone();
        let runtime = shared.runtime.clone();
        runtime.spawn(async move {
            let worker_id = format!("transform-{}", index);
            loop {
                let job = {
                    let mut guard = rx.lock().await;
                    guard.recv().await
                };

                let Some(job) = job else {
                    break;
                };

                let text = normalize_for_tts(&job.raw_content);
                let result = StageResult::TransformationOk {
                    message_id: job.message_id,
                    worker_id: worker_id.clone(),
                    text,
                };

                if command_tx
                    .send(PipelineCommand::StageResult(result))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    StageHandle { sender: tx }
}

fn spawn_synthesis_stage(
    shared: Arc<PipelineShared>,
    command_tx: mpsc::Sender<PipelineCommand>,
) -> StageHandle<SynthesisJob> {
    let (tx, rx) = mpsc::channel::<SynthesisJob>(SYNTHESIS_QUEUE_SIZE);
    let rx = Arc::new(Mutex::new(rx));

    for index in 0..SYNTHESIS_WORKERS {
        let rx = rx.clone();
        let command_tx = command_tx.clone();
        let shared = shared.clone();
        let runtime = shared.runtime.clone();
        runtime.spawn(async move {
            let worker_id = format!("synth-{}", index);

            loop {
                let job = {
                    let mut guard = rx.lock().await;
                    guard.recv().await
                };

                let Some(job) = job else {
                    break;
                };

                let outcome = process_synthesis_job(&shared, &worker_id, job).await;
                if command_tx
                    .send(PipelineCommand::StageResult(outcome))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    StageHandle { sender: tx }
}

fn spawn_playback_stage(
    shared: Arc<PipelineShared>,
    command_tx: mpsc::Sender<PipelineCommand>,
) -> StageHandle<PlaybackJob> {
    let (tx, rx) = mpsc::channel::<PlaybackJob>(PLAYBACK_QUEUE_SIZE);
    let rx = Arc::new(Mutex::new(rx));

    for index in 0..PLAYBACK_WORKERS {
        let rx = rx.clone();
        let command_tx = command_tx.clone();
        let shared = shared.clone();
        let runtime = shared.runtime.clone();
        runtime.spawn(async move {
            let worker_id = format!("playback-{}", index);
            loop {
                let job = {
                    let mut guard = rx.lock().await;
                    guard.recv().await
                };

                let Some(job) = job else {
                    break;
                };

                if command_tx
                    .send(PipelineCommand::StageResult(StageResult::PlaybackStarted {
                        message_id: job.message_id,
                        worker_id: worker_id.clone(),
                        playback_token: job.playback_token,
                    }))
                    .await
                    .is_err()
                {
                    break;
                }

                let result = process_playback_job(&shared, &worker_id, job).await;
                if command_tx
                    .send(PipelineCommand::StageResult(result))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    StageHandle { sender: tx }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FilterMode {
    #[default]
    Blacklist,
    Whitelist,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CommandAcceptance {
    Nobody,
    #[default]
    Moderators,
    ChannelOwner,
}

#[derive(Clone, Debug, Deserialize)]
struct FilterRulesConfig {
    #[serde(default = "default_ignore_prefix")]
    ignore_prefix: String,
    #[serde(default = "default_command_prefix")]
    command_prefix: String,
    #[serde(default = "default_true")]
    suppress_provider_bots: bool,
    #[serde(default)]
    ignore_paid: bool,
}

impl Default for FilterRulesConfig {
    fn default() -> Self {
        Self {
            ignore_prefix: default_ignore_prefix(),
            command_prefix: default_command_prefix(),
            suppress_provider_bots: true,
            ignore_paid: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct MessageFilteringConfig {
    #[serde(default, rename = "mode")]
    _mode: FilterMode,
    #[serde(default)]
    rules: FilterRulesConfig,
}

impl Default for MessageFilteringConfig {
    fn default() -> Self {
        Self {
            _mode: FilterMode::Blacklist,
            rules: FilterRulesConfig::default(),
        }
    }
}

#[derive(Clone, Debug)]
struct FilterPolicy {
    rules: FilterRulesConfig,
    command_acceptance: CommandAcceptance,
}

impl Default for FilterPolicy {
    fn default() -> Self {
        Self {
            rules: FilterRulesConfig::default(),
            command_acceptance: CommandAcceptance::Moderators,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FilterOutcome {
    Pass,
    Skip {
        reason: SkipReason,
        note: Option<String>,
        control_event: Option<PipelineControlEvent>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedCommand {
    raw: String,
    command: String,
    args: Vec<String>,
}

fn process_filtering_job(
    shared: &PipelineShared,
    worker_id: &str,
    job: FilteringJob,
) -> StageResult {
    let policy = load_filter_policy(&shared.settings);
    match evaluate_filter(&job.header, &policy, job.message_id) {
        FilterOutcome::Pass => StageResult::FilteringPassed {
            message_id: job.message_id,
            worker_id: worker_id.to_string(),
        },
        FilterOutcome::Skip {
            reason,
            note,
            control_event,
        } => StageResult::FilteringSkipped {
            message_id: job.message_id,
            worker_id: worker_id.to_string(),
            reason,
            note,
            control_event,
        },
    }
}

fn load_filter_policy(settings: &SettingsService) -> FilterPolicy {
    let mut policy = FilterPolicy::default();

    match settings.get("message_filtering") {
        Ok(record) => match serde_json::from_value::<MessageFilteringConfig>(record.value) {
            Ok(config) => policy.rules = config.rules,
            Err(err) => warn!("invalid message_filtering setting payload: {err}"),
        },
        Err(err) => warn!("failed to load message_filtering setting: {err}"),
    }

    match settings.get("command_acceptance") {
        Ok(record) => match serde_json::from_value::<CommandAcceptance>(record.value) {
            Ok(value) => policy.command_acceptance = value,
            Err(err) => warn!("invalid command_acceptance setting payload: {err}"),
        },
        Err(err) => warn!("failed to load command_acceptance setting: {err}"),
    }

    policy
}

fn evaluate_filter(
    header: &MessageHeader,
    policy: &FilterPolicy,
    message_id: u64,
) -> FilterOutcome {
    if metadata_bool(header, "operator_marked_skip") {
        return FilterOutcome::Skip {
            reason: SkipReason::OperatorOverride,
            note: Some("operator_marked_skip".to_string()),
            control_event: None,
        };
    }

    if is_operator_force_include_header(header) {
        return FilterOutcome::Pass;
    }

    if policy.rules.suppress_provider_bots && metadata_bool(header, "provider_bot") {
        return FilterOutcome::Skip {
            reason: SkipReason::ProviderBotSuppressed,
            note: Some("provider_bot".to_string()),
            control_event: None,
        };
    }

    if policy.rules.ignore_paid && matches!(header.delivery_kind, DeliveryKind::Paid) {
        return FilterOutcome::Skip {
            reason: SkipReason::PaidMessageSuppressed,
            note: Some("ignore_paid".to_string()),
            control_event: None,
        };
    }

    if let Some(command) = parse_command(&header.content, &policy.rules.command_prefix) {
        if command_is_allowed(header, policy.command_acceptance) {
            let control_event = PipelineControlEvent::ChatCommand(ChatCommandEvent {
                message_id,
                provider: header.provider.clone(),
                provider_message_id: header.provider_message_id.clone(),
                issuer_display_name: header.sender.clone(),
                issuer_user_id: header.user_id.clone(),
                command: command.command.clone(),
                args: command.args.clone(),
                raw: command.raw.clone(),
            });
            return FilterOutcome::Skip {
                reason: SkipReason::CommandFiltered,
                note: Some("command_accepted".to_string()),
                control_event: Some(control_event),
            };
        }

        return FilterOutcome::Skip {
            reason: SkipReason::CommandRejected,
            note: Some("command_not_permitted".to_string()),
            control_event: None,
        };
    }

    if starts_with_prefix(&header.content, &policy.rules.ignore_prefix) {
        return FilterOutcome::Skip {
            reason: SkipReason::FilterRule,
            note: Some(format!("ignore_prefix:{}", policy.rules.ignore_prefix)),
            control_event: None,
        };
    }

    FilterOutcome::Pass
}

fn command_is_allowed(header: &MessageHeader, acceptance: CommandAcceptance) -> bool {
    let is_channel_owner = metadata_bool(header, "is_channel_owner");
    let is_moderator = is_channel_owner || metadata_bool(header, "is_moderator");

    match acceptance {
        CommandAcceptance::Nobody => false,
        CommandAcceptance::Moderators => is_moderator,
        CommandAcceptance::ChannelOwner => is_channel_owner,
    }
}

fn parse_command(content: &str, command_prefix: &str) -> Option<ParsedCommand> {
    if command_prefix.trim().is_empty() {
        return None;
    }
    let trimmed = content.trim_start();
    if !trimmed.starts_with(command_prefix) {
        return None;
    }

    let suffix = &trimmed[command_prefix.len()..];
    if !(suffix.is_empty() || suffix.starts_with(char::is_whitespace)) {
        return None;
    }

    let raw = suffix.trim().to_string();
    let mut parts = raw.split_whitespace();
    let command = parts.next().unwrap_or("").to_ascii_lowercase();
    let args = parts.map(ToString::to_string).collect::<Vec<_>>();
    Some(ParsedCommand { raw, command, args })
}

fn starts_with_prefix(content: &str, prefix: &str) -> bool {
    if prefix.trim().is_empty() {
        return false;
    }
    content.trim_start().starts_with(prefix)
}

fn metadata_bool(header: &MessageHeader, key: &str) -> bool {
    header
        .metadata
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn is_operator_force_include(snapshot: &MessageSnapshot) -> bool {
    is_operator_force_include_header(&snapshot.header)
}

fn is_operator_force_include_header(header: &MessageHeader) -> bool {
    metadata_bool(header, "operator_force_include") || metadata_bool(header, "operator_override")
}

fn default_ignore_prefix() -> String {
    "!".to_string()
}

fn default_command_prefix() -> String {
    "!lupo".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
struct AdvancedSettingsConfig {
    #[serde(default)]
    scheduler: SchedulerConfig,
}

#[derive(Clone, Debug, Deserialize)]
struct SchedulerConfig {
    #[serde(default = "default_max_predicted_queue_duration_secs")]
    max_predicted_queue_duration_secs: u64,
}

impl Default for AdvancedSettingsConfig {
    fn default() -> Self {
        Self {
            scheduler: SchedulerConfig::default(),
        }
    }
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_predicted_queue_duration_secs: default_max_predicted_queue_duration_secs(),
        }
    }
}

fn default_max_predicted_queue_duration_secs() -> u64 {
    DEFAULT_MAX_PREDICTED_QUEUE_DURATION_SECS
}

fn load_max_predicted_queue_duration_ms(settings: &SettingsService) -> u64 {
    let mut seconds = default_max_predicted_queue_duration_secs();
    match settings.get("advanced") {
        Ok(record) => match serde_json::from_value::<AdvancedSettingsConfig>(record.value) {
            Ok(config) => {
                seconds = config.scheduler.max_predicted_queue_duration_secs;
            }
            Err(err) => warn!("invalid advanced settings payload for scheduler: {err}"),
        },
        Err(err) => warn!("failed to load advanced settings for scheduler: {err}"),
    }
    seconds.saturating_mul(1000)
}

fn load_runtime_state(
    db: &lupo_persistence::DatabaseHandle,
) -> Result<PersistedPipelineRuntimeState, lupo_persistence::Error> {
    match load_app_state_json(db, PIPELINE_RUNTIME_STATE_KEY)? {
        Some(value) => match serde_json::from_value::<PersistedPipelineRuntimeState>(value) {
            Ok(state) => Ok(state),
            Err(err) => {
                warn!("invalid persisted pipeline runtime state, resetting: {err}");
                Ok(PersistedPipelineRuntimeState::default())
            }
        },
        None => Ok(PersistedPipelineRuntimeState::default()),
    }
}

fn save_runtime_state(
    db: &lupo_persistence::DatabaseHandle,
    state: &PersistedPipelineRuntimeState,
) -> Result<(), lupo_persistence::Error> {
    if state.playback_enabled
        && state.queue_cursor.is_none()
        && state.pinned_next.is_none()
        && state.forced_queue.is_empty()
    {
        delete_app_state_json(db, PIPELINE_RUNTIME_STATE_KEY)?;
        return Ok(());
    }

    let value = serde_json::to_value(state)?;
    save_app_state_json(db, PIPELINE_RUNTIME_STATE_KEY, &value)?;
    Ok(())
}

fn chat_command_to_action(command: &ChatCommandEvent) -> Option<ControlAction> {
    match command.command.as_str() {
        "skip" => {
            parse_message_id_arg(command).map(|message_id| ControlAction::Skip { message_id })
        }
        "retry" => {
            parse_message_id_arg(command).map(|message_id| ControlAction::Retry { message_id })
        }
        "unskip" => {
            parse_message_id_arg(command).map(|message_id| ControlAction::Unskip { message_id })
        }
        "pin-next" | "pin_next" => {
            parse_message_id_arg(command).map(|message_id| ControlAction::PinNext { message_id })
        }
        "force-play" | "force_play" => {
            parse_message_id_arg(command).map(|message_id| ControlAction::ForcePlay { message_id })
        }
        "stop" | "stop-now" | "stop_now" => Some(ControlAction::StopNow),
        "play" | "resume" => Some(ControlAction::SetPlaybackEnabled { enabled: true }),
        "pause" => Some(ControlAction::SetPlaybackEnabled { enabled: false }),
        _ => None,
    }
}

fn parse_message_id_arg(command: &ChatCommandEvent) -> Option<u64> {
    command.args.first().and_then(|arg| arg.parse::<u64>().ok())
}

fn retry_state_from_error(error: &ErrorState, snapshot: &MessageSnapshot) -> Option<MessageState> {
    match error.stage {
        Some(PipelineStage::Filtering) | Some(PipelineStage::Ingestion) | None => {
            Some(MessageState::Arrived)
        }
        Some(PipelineStage::Transformation) => Some(MessageState::Filtered),
        Some(PipelineStage::Synthesis) => Some(MessageState::Transformed(TransformedState {
            completed_at: OffsetDateTime::now_utc(),
            text: text_for_resynthesis(snapshot),
        })),
        Some(PipelineStage::Playback) => replay_target_state(snapshot),
    }
}

fn unskip_target_state(snapshot: &MessageSnapshot) -> Option<MessageState> {
    replay_target_state(snapshot).or(Some(MessageState::Arrived))
}

fn replay_target_state(snapshot: &MessageSnapshot) -> Option<MessageState> {
    synthesized_state_from_snapshot(snapshot).map(MessageState::Synthesized)
}

fn synthesized_state_from_snapshot(snapshot: &MessageSnapshot) -> Option<SynthesizedState> {
    match snapshot.artifacts.get("synth") {
        Some(Artifact::Audio(audio)) => Some(SynthesizedState {
            artifact_key: audio.storage_key.clone(),
            backend: audio.backend.clone(),
            duration_ms: audio.duration_ms,
            completed_at: OffsetDateTime::now_utc(),
            sha256: audio.sha256.clone(),
        }),
        _ => None,
    }
}

fn text_for_resynthesis(snapshot: &MessageSnapshot) -> String {
    match &snapshot.lifecycle.state {
        MessageState::Transformed(transformed) => transformed.text.clone(),
        _ => snapshot.header.content.clone(),
    }
}

fn should_ignore_moderator_action(snapshot: &MessageSnapshot, source: CommandSource) -> bool {
    matches!(source, CommandSource::Moderator) && has_operator_override(snapshot)
}

fn has_operator_override(snapshot: &MessageSnapshot) -> bool {
    metadata_bool(&snapshot.header, "operator_override")
        || metadata_bool(&snapshot.header, "operator_force_include")
        || metadata_bool(&snapshot.header, "operator_marked_skip")
}

fn mark_override_metadata(
    snapshot: &mut MessageSnapshot,
    source: CommandSource,
    marked_skip: bool,
) {
    match source {
        CommandSource::Operator => {
            snapshot
                .header
                .metadata
                .insert("operator_override".to_string(), Value::Bool(true));
            if marked_skip {
                snapshot
                    .header
                    .metadata
                    .insert("operator_marked_skip".to_string(), Value::Bool(true));
            }
        }
        CommandSource::Moderator => {
            snapshot
                .header
                .metadata
                .insert("moderator_override".to_string(), Value::Bool(true));
        }
    }
}

fn queue_priority_rank(priority: &QueuePriority) -> u8 {
    match priority {
        QueuePriority::Normal => 0,
        QueuePriority::High => 1,
        QueuePriority::Urgent => 2,
    }
}

fn queue_priority_for_snapshot(snapshot: &MessageSnapshot) -> QueuePriority {
    if metadata_bool(&snapshot.header, "operator_force_include") {
        QueuePriority::Urgent
    } else if metadata_bool(&snapshot.header, "operator_pin_next") {
        QueuePriority::High
    } else {
        QueuePriority::Normal
    }
}

fn estimate_duration_ms(text: &str) -> u32 {
    let grapheme_count = text.chars().count() as u64;
    let estimated = grapheme_count.saturating_mul(ESTIMATED_DURATION_MS_PER_CHAR as u64);
    estimated.clamp(
        MIN_ESTIMATED_DURATION_MS as u64,
        MAX_ESTIMATED_DURATION_MS as u64,
    ) as u32
}

fn snapshot_actual_duration_ms(snapshot: &MessageSnapshot) -> Option<u32> {
    match &snapshot.lifecycle.state {
        MessageState::Synthesized(state) => Some(state.duration_ms),
        MessageState::Played(state) => Some(state.duration_ms),
        _ => match snapshot.artifacts.get("synth") {
            Some(Artifact::Audio(audio)) => Some(audio.duration_ms),
            _ => None,
        },
    }
}

async fn process_synthesis_job(
    shared: &PipelineShared,
    worker_id: &str,
    job: SynthesisJob,
) -> StageResult {
    let api_key = match load_secret_string(&shared.settings, OPENAI_SECRET_KEY) {
        Ok(Some(key)) => key,
        Ok(None) => {
            return StageResult::SynthesisErr {
                message_id: job.message_id,
                worker_id: worker_id.to_string(),
                attempt: job.attempt,
                error: StageError::non_retryable(
                    "missing_api_key",
                    Some("OpenAI API key is not configured".into()),
                ),
            };
        }
        Err(err) => {
            return StageResult::SynthesisErr {
                message_id: job.message_id,
                worker_id: worker_id.to_string(),
                attempt: job.attempt,
                error: StageError::non_retryable("secret_store_error", Some(err.to_string())),
            };
        }
    };

    let client = OpenAi::new(api_key);
    let audio_bytes = match client.text_to_speech(job.text.clone()).await {
        Ok(bytes) => bytes.to_vec(),
        Err(err) => {
            return StageResult::SynthesisErr {
                message_id: job.message_id,
                worker_id: worker_id.to_string(),
                attempt: job.attempt,
                error: StageError::retryable("tts_failed", Some(err.to_string())),
            };
        }
    };

    let duration_ms = wav_duration_ms(audio_bytes.as_slice()).unwrap_or(0);
    let sha256 = sha256_hex(audio_bytes.as_slice());
    let artifact_key = artifact_storage_key(job.message_id, job.attempt);

    StageResult::SynthesisOk {
        message_id: job.message_id,
        worker_id: worker_id.to_string(),
        attempt: job.attempt,
        artifact_key,
        audio_bytes,
        duration_ms,
        sha256,
    }
}

async fn process_playback_job(
    shared: &PipelineShared,
    worker_id: &str,
    job: PlaybackJob,
) -> StageResult {
    let Some(bytes) = load_audio_artifact(&shared.db, job.message_id, &job.artifact_key)
        .ok()
        .flatten()
    else {
        return StageResult::PlaybackErr {
            message_id: job.message_id,
            worker_id: worker_id.to_string(),
            error: StageError::non_retryable(
                "missing_artifact",
                Some(format!(
                    "no artifact {} for message {}",
                    job.artifact_key, job.message_id
                )),
            ),
            playback_token: job.playback_token,
        };
    };

    if let Err(err) = shared.audio.play_wav_and_wait(bytes.clone()).await {
        return StageResult::PlaybackErr {
            message_id: job.message_id,
            worker_id: worker_id.to_string(),
            error: StageError::non_retryable("playback_failed", Some(err.to_string())),
            playback_token: job.playback_token,
        };
    }

    StageResult::PlaybackOk {
        message_id: job.message_id,
        worker_id: worker_id.to_string(),
        duration_ms: job.duration_ms,
        playback_token: job.playback_token,
    }
}

fn normalize_for_tts(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        "[empty message]".to_string()
    } else {
        normalized
    }
}

fn wav_duration_ms(bytes: &[u8]) -> Option<u32> {
    let cursor = Cursor::new(bytes);
    let reader = WavReader::new(cursor).ok()?;
    let spec = reader.spec();
    if spec.sample_rate == 0 || spec.channels == 0 {
        return None;
    }
    let total_samples = reader.duration() as u64;
    let channels = spec.channels as u64;
    if channels == 0 {
        return None;
    }
    let frames = total_samples / channels;
    let duration_ms = (frames as f64 / spec.sample_rate as f64) * 1000.0;
    Some(duration_ms.round().clamp(0.0, u32::MAX as f64) as u32)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(hasher.finalize())
}

fn artifact_storage_key(message_id: u64, attempt: u32) -> String {
    let timestamp = OffsetDateTime::now_utc().unix_timestamp_nanos();
    format!("{message_id}-{attempt}-{timestamp}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn header(content: &str) -> MessageHeader {
        MessageHeader {
            id: 1,
            provider: "twitch_chat".to_string(),
            provider_message_id: Some("provider-msg-1".to_string()),
            channel_id: Some("channel-1".to_string()),
            user_id: Some("user-1".to_string()),
            sender: "alice".to_string(),
            content: content.to_string(),
            source_timestamp: Some(OffsetDateTime::now_utc()),
            received_at: OffsetDateTime::now_utc(),
            delivery_kind: DeliveryKind::Standard,
            payment: None,
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn parse_command_respects_prefix_boundary() {
        let command = parse_command("!lupo skip 42", "!lupo").expect("expected command");
        assert_eq!(command.command, "skip");
        assert_eq!(command.args, vec!["42".to_string()]);

        assert!(parse_command("!lupobot", "!lupo").is_none());
    }

    #[test]
    fn filter_emits_control_for_allowed_command() {
        let mut message = header("!lupo skip 42");
        message
            .metadata
            .insert("is_moderator".to_string(), json!(true));

        let policy = FilterPolicy::default();
        let result = evaluate_filter(&message, &policy, 99);
        match result {
            FilterOutcome::Skip {
                reason,
                control_event,
                ..
            } => {
                assert_eq!(reason, SkipReason::CommandFiltered);
                assert!(matches!(
                    control_event,
                    Some(PipelineControlEvent::ChatCommand(_))
                ));
            }
            _ => panic!("expected command to be filtered"),
        }
    }

    #[test]
    fn filter_rejects_command_when_sender_not_permitted() {
        let message = header("!lupo retry 11");
        let policy = FilterPolicy::default();
        let result = evaluate_filter(&message, &policy, 1);
        assert!(matches!(
            result,
            FilterOutcome::Skip {
                reason: SkipReason::CommandRejected,
                control_event: None,
                ..
            }
        ));
    }

    #[test]
    fn command_acceptance_matrix_is_enforced() {
        let mut moderator = header("!lupo retry 7");
        moderator
            .metadata
            .insert("is_moderator".to_string(), json!(true));

        let mut owner = header("!lupo retry 7");
        owner
            .metadata
            .insert("is_channel_owner".to_string(), json!(true));

        let plain = header("!lupo retry 7");

        assert!(!command_is_allowed(&plain, CommandAcceptance::Nobody));
        assert!(!command_is_allowed(&plain, CommandAcceptance::Moderators));
        assert!(!command_is_allowed(&plain, CommandAcceptance::ChannelOwner));
        assert!(command_is_allowed(
            &moderator,
            CommandAcceptance::Moderators
        ));
        assert!(!command_is_allowed(
            &moderator,
            CommandAcceptance::ChannelOwner
        ));
        assert!(command_is_allowed(&owner, CommandAcceptance::ChannelOwner));
    }

    #[test]
    fn filter_applies_prefix_bot_and_paid_rules() {
        let policy = FilterPolicy::default();

        let prefix_message = header("!hello");
        assert!(matches!(
            evaluate_filter(&prefix_message, &policy, 1),
            FilterOutcome::Skip {
                reason: SkipReason::FilterRule,
                ..
            }
        ));

        let mut bot_message = header("normal");
        bot_message
            .metadata
            .insert("provider_bot".to_string(), json!(true));
        assert!(matches!(
            evaluate_filter(&bot_message, &policy, 1),
            FilterOutcome::Skip {
                reason: SkipReason::ProviderBotSuppressed,
                ..
            }
        ));

        let mut paid_policy = FilterPolicy::default();
        paid_policy.rules.ignore_paid = true;
        let mut paid_message = header("thanks");
        paid_message.delivery_kind = DeliveryKind::Paid;
        assert!(matches!(
            evaluate_filter(&paid_message, &paid_policy, 1),
            FilterOutcome::Skip {
                reason: SkipReason::PaidMessageSuppressed,
                ..
            }
        ));
    }

    #[test]
    fn operator_force_include_overrides_filters() {
        let mut message = header("!lupo skip 42");
        message
            .metadata
            .insert("operator_force_include".to_string(), json!(true));

        let policy = FilterPolicy::default();
        assert!(matches!(
            evaluate_filter(&message, &policy, 1),
            FilterOutcome::Pass
        ));
    }

    #[test]
    fn chat_commands_map_to_control_actions() {
        let base = ChatCommandEvent {
            message_id: 99,
            provider: "twitch_chat".to_string(),
            provider_message_id: Some("abc".to_string()),
            issuer_display_name: "mod".to_string(),
            issuer_user_id: Some("u1".to_string()),
            command: "skip".to_string(),
            args: vec!["42".to_string()],
            raw: "skip 42".to_string(),
        };

        assert!(matches!(
            chat_command_to_action(&base),
            Some(ControlAction::Skip { message_id: 42 })
        ));

        let mut pin = base.clone();
        pin.command = "pin-next".to_string();
        assert!(matches!(
            chat_command_to_action(&pin),
            Some(ControlAction::PinNext { message_id: 42 })
        ));

        let mut stop = base.clone();
        stop.command = "stop".to_string();
        stop.args.clear();
        assert!(matches!(
            chat_command_to_action(&stop),
            Some(ControlAction::StopNow)
        ));
    }

    #[test]
    fn estimate_duration_applies_bounds() {
        let short = estimate_duration_ms("a");
        let long = estimate_duration_ms(&"a".repeat(10_000));
        assert_eq!(short, MIN_ESTIMATED_DURATION_MS);
        assert_eq!(long, MAX_ESTIMATED_DURATION_MS);
    }

    #[test]
    fn queue_priority_is_derived_from_override_metadata() {
        let mut snapshot = MessageSnapshot {
            header: header("hello"),
            lifecycle: lupo_persistence::Lifecycle::new(MessageState::Arrived),
            artifacts: lupo_persistence::ArtifactSet::default(),
        };
        assert!(matches!(
            queue_priority_for_snapshot(&snapshot),
            QueuePriority::Normal
        ));

        snapshot
            .header
            .metadata
            .insert("operator_pin_next".to_string(), json!(true));
        assert!(matches!(
            queue_priority_for_snapshot(&snapshot),
            QueuePriority::High
        ));

        snapshot
            .header
            .metadata
            .insert("operator_force_include".to_string(), json!(true));
        assert!(matches!(
            queue_priority_for_snapshot(&snapshot),
            QueuePriority::Urgent
        ));

        assert!(
            queue_priority_rank(&QueuePriority::Urgent) > queue_priority_rank(&QueuePriority::High)
        );
    }
}

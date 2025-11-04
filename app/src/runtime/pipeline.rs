// SPDX-License-Identifier: Apache-2.0
#![allow(
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::large_enum_variant,
    clippy::redundant_guards,
    clippy::too_many_arguments
)]

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io::Cursor;
use std::pin::Pin;
use std::sync::Arc;

use hex::encode as hex_encode;
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use lupo_audio::Audio;
use lupo_configuration::SettingsService;
use lupo_persistence::{
    Artifact, AudioArtifact, AudioFormat, DeliveryKind, ErrorState, MessageHeader, MessageSnapshot,
    MessageState, MessageStateKind, NewMessage, PipelineStage, PlayedState, PlayingState,
    QueuePriority, QueuedState, SkipReason, SkippedState, SynthBackendId, SynthesizedState,
    SynthesizingState, TransformedState, TransformingState, TransitionCause, canonical_chatter_id,
    clear_all_messages, delete_app_state_json, enqueue_message, list_messages_desc,
    load_app_state_json, load_audio_artifact, load_chatter_override, save_app_state_json,
    save_message_snapshot, store_audio_artifact,
};
use lupo_secrets::load_secret_string;
#[cfg(target_os = "macos")]
use lupo_tts_macos::{SystemSpeechProcessConfig, SystemSpeechProcessManager};
use lupo_tts_openai::{OpenAi, SpeechOptions as OpenAiSpeechOptions};
use lupo_tts_parler::{ParlerLoadConfig, ParlerSynthesizer};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::runtime::Runtime;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, oneshot};
use tracing::{debug, error, info, warn};

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
const PARLER_BACKEND: &str = "parler";
const SYSTEM_TTS_BACKEND: &str = "system_tts";
const ULTRA_LIGHT_BACKEND: &str = "ultra_light";
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
    ProviderStatusesChanged(Vec<RuntimeTtsProviderStatus>),
    Cleared,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PipelinePlaybackState {
    pub enabled: bool,
    pub active_message_id: Option<u64>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RuntimeTtsProviderStatus {
    pub provider_id: String,
    pub label: String,
    pub state: RuntimeTtsProviderState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeTtsProviderState {
    Ready,
    Unavailable,
    Error,
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
    tts_providers: HashMap<String, Arc<dyn RuntimeTtsProvider>>,
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
        voice_override: Option<String>,
        backend: SynthBackendId,
        backend_label: String,
        synthesis_parameters: ProviderOptions,
        provider_statuses: Vec<RuntimeTtsProviderStatus>,
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
        provider_statuses: Vec<RuntimeTtsProviderStatus>,
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

type ProviderOptions = BTreeMap<String, Value>;

#[derive(Clone, Debug, Default)]
struct ProviderSynthesisResult {
    audio_bytes: Vec<u8>,
    parameters: ProviderOptions,
}

type SynthesisFuture =
    Pin<Box<dyn Future<Output = Result<ProviderSynthesisResult, StageError>> + Send>>;

trait RuntimeTtsProvider: Send + Sync {
    fn provider_id(&self) -> &'static str;
    fn label(&self) -> &'static str;
    fn readiness_detail(&self, settings: &SettingsService) -> Option<String>;
    fn synthesize(
        &self,
        settings: SettingsService,
        text: String,
        options: ProviderOptions,
    ) -> SynthesisFuture;
}

struct OpenAiRuntimeTtsProvider;

impl RuntimeTtsProvider for OpenAiRuntimeTtsProvider {
    fn provider_id(&self) -> &'static str {
        OPENAI_BACKEND
    }

    fn label(&self) -> &'static str {
        "OpenAI"
    }

    fn readiness_detail(&self, settings: &SettingsService) -> Option<String> {
        match load_secret_string(settings, OPENAI_SECRET_KEY) {
            Ok(Some(_)) => None,
            Ok(None) => Some("API key missing".to_string()),
            Err(err) => Some(format!("Secret store error: {err}")),
        }
    }

    fn synthesize(
        &self,
        settings: SettingsService,
        text: String,
        options: ProviderOptions,
    ) -> SynthesisFuture {
        Box::pin(async move {
            let api_key = match load_secret_string(&settings, OPENAI_SECRET_KEY) {
                Ok(Some(key)) => key,
                Ok(None) => {
                    return Err(StageError::non_retryable(
                        "missing_api_key",
                        Some("OpenAI API key is not configured".into()),
                    ));
                }
                Err(err) => {
                    return Err(StageError::non_retryable(
                        "secret_store_error",
                        Some(err.to_string()),
                    ));
                }
            };

            let (speech_options, parameters) = parse_openai_speech_options(&options);
            let client = OpenAi::new(api_key);
            client
                .text_to_speech_with_options(text, &speech_options)
                .await
                .map(|bytes| ProviderSynthesisResult {
                    audio_bytes: bytes.to_vec(),
                    parameters,
                })
                .map_err(|err| StageError::retryable("tts_failed", Some(err.to_string())))
        })
    }
}

struct ParlerRuntimeTtsProvider;

impl RuntimeTtsProvider for ParlerRuntimeTtsProvider {
    fn provider_id(&self) -> &'static str {
        PARLER_BACKEND
    }

    fn label(&self) -> &'static str {
        "Parler"
    }

    fn readiness_detail(&self, settings: &SettingsService) -> Option<String> {
        let strategy = load_offline_tts_strategy(settings);
        if strategy.engine.eq_ignore_ascii_case(PARLER_BACKEND) {
            None
        } else {
            Some(format!(
                "offline_tts_strategy.engine is '{}' (expected 'parler')",
                strategy.engine
            ))
        }
    }

    fn synthesize(
        &self,
        _settings: SettingsService,
        text: String,
        options: ProviderOptions,
    ) -> SynthesisFuture {
        Box::pin(async move {
            let load_config = parse_parler_load_config(&options);
            let parameters = parler_parameters(&load_config);
            tokio::task::spawn_blocking(move || {
                let mut synthesizer = ParlerSynthesizer::new(load_config).map_err(|err| {
                    StageError::retryable("parler_init_failed", Some(err.to_string()))
                })?;

                synthesizer
                    .synthesize_to_wav(&text, None, None)
                    .map(|audio_bytes| ProviderSynthesisResult {
                        audio_bytes,
                        parameters,
                    })
                    .map_err(|err| {
                        StageError::retryable("parler_synthesize_failed", Some(err.to_string()))
                    })
            })
            .await
            .map_err(|err| StageError::retryable("parler_join_failed", Some(err.to_string())))?
        })
    }
}

#[cfg(target_os = "macos")]
struct SystemRuntimeTtsProvider {
    manager: Arc<SystemSpeechProcessManager>,
}

#[cfg(target_os = "macos")]
impl RuntimeTtsProvider for SystemRuntimeTtsProvider {
    fn provider_id(&self) -> &'static str {
        SYSTEM_TTS_BACKEND
    }

    fn label(&self) -> &'static str {
        "System TTS"
    }

    fn readiness_detail(&self, _settings: &SettingsService) -> Option<String> {
        self.manager
            .resolve_helper_path()
            .err()
            .map(|err| err.to_string())
    }

    fn synthesize(
        &self,
        _settings: SettingsService,
        text: String,
        _options: ProviderOptions,
    ) -> SynthesisFuture {
        let manager = Arc::clone(&self.manager);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || manager.synthesize_to_wav(&text))
                .await
                .map_err(|err| StageError::retryable("tts_join_failed", Some(err.to_string())))?
                .map(|audio_bytes| ProviderSynthesisResult {
                    audio_bytes,
                    parameters: BTreeMap::new(),
                })
                .map_err(|err| StageError::retryable("tts_failed", Some(err.to_string())))
        })
    }
}

struct UltraLightRuntimeTtsProvider;

impl RuntimeTtsProvider for UltraLightRuntimeTtsProvider {
    fn provider_id(&self) -> &'static str {
        ULTRA_LIGHT_BACKEND
    }

    fn label(&self) -> &'static str {
        "Ultra-light"
    }

    fn readiness_detail(&self, settings: &SettingsService) -> Option<String> {
        let strategy = load_offline_tts_strategy(settings);
        if strategy.enable_ultra_light {
            None
        } else {
            Some("offline_tts_strategy.enable_ultra_light is false".to_string())
        }
    }

    fn synthesize(
        &self,
        _settings: SettingsService,
        text: String,
        _options: ProviderOptions,
    ) -> SynthesisFuture {
        Box::pin(async move {
            synthesize_ultra_light_wav(&text)
                .map_err(|err| {
                    StageError::non_retryable("ultra_light_encode_failed", Some(err.to_string()))
                })
                .map(|audio_bytes| ProviderSynthesisResult {
                    audio_bytes,
                    parameters: ProviderOptions::from([
                        ("sample_rate_hz".to_string(), Value::from(16_000_u64)),
                        ("bits_per_sample".to_string(), Value::from(16_u64)),
                        ("channels".to_string(), Value::from(1_u64)),
                    ]),
                })
        })
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
    voice_override: Option<String>,
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
        let mut tts_providers: HashMap<String, Arc<dyn RuntimeTtsProvider>> = HashMap::new();
        let openai_provider: Arc<dyn RuntimeTtsProvider> = Arc::new(OpenAiRuntimeTtsProvider);
        tts_providers.insert(openai_provider.provider_id().to_string(), openai_provider);
        let parler_provider: Arc<dyn RuntimeTtsProvider> = Arc::new(ParlerRuntimeTtsProvider);
        tts_providers.insert(parler_provider.provider_id().to_string(), parler_provider);
        let ultra_light_provider: Arc<dyn RuntimeTtsProvider> =
            Arc::new(UltraLightRuntimeTtsProvider);
        tts_providers.insert(
            ultra_light_provider.provider_id().to_string(),
            ultra_light_provider,
        );
        #[cfg(target_os = "macos")]
        {
            match SystemSpeechProcessManager::new(SystemSpeechProcessConfig::default()) {
                Ok(manager) => {
                    let system_provider: Arc<dyn RuntimeTtsProvider> =
                        Arc::new(SystemRuntimeTtsProvider {
                            manager: Arc::new(manager),
                        });
                    tts_providers
                        .insert(system_provider.provider_id().to_string(), system_provider);
                }
                Err(err) => {
                    warn!(
                        "failed to initialize macOS system TTS helper manager; provider disabled: {err}"
                    );
                }
            }
        }

        let shared = Arc::new(PipelineShared {
            runtime: runtime.clone(),
            db: db.clone(),
            audio: audio.clone(),
            settings: settings.clone(),
            tts_providers,
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
    pub async fn set_output_volume(&self, volume: f32) -> Result<f32, PipelineError> {
        self.shared
            .audio
            .set_volume(volume)
            .await
            .map_err(PipelineError::Audio)
    }

    pub fn set_output_volume_blocking(&self, volume: f32) -> Result<f32, PipelineError> {
        let shared = self.shared.clone();
        let runtime = shared.runtime.clone();
        runtime_block_on(
            runtime,
            async move { shared.audio.set_volume(volume).await },
        )
        .map_err(|_err| PipelineError::Audio(lupo_audio::Error::ResponseDropped))?
        .map_err(PipelineError::Audio)
    }

    #[allow(dead_code)]
    pub async fn output_volume(&self) -> Result<f32, PipelineError> {
        self.shared
            .audio
            .volume()
            .await
            .map_err(PipelineError::Audio)
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
        match runtime_block_on(runtime, async move {
            MessagePipeline::snapshots_inner(&shared).await
        }) {
            Ok(snapshots) => snapshots,
            Err(err) => {
                error!(%err, "failed to read pipeline snapshots");
                Vec::new()
            }
        }
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
            if let Err(err) = runtime_block_on(runtime, async move {
                let mut state = shared.state.write().await;
                state.clear();
                for snapshot in &seed {
                    state.insert(snapshot.header.id, snapshot.clone());
                }
            }) {
                error!(%err, "failed to seed in-memory pipeline state");
            }
        }

        let command_tx = self.command_tx.clone();
        let runtime = self.shared.runtime.clone();
        match runtime_block_on(runtime, async move {
            for snapshot in snapshots {
                if command_tx
                    .send(PipelineCommand::Register(snapshot))
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            true
        }) {
            Ok(true) => {}
            Ok(false) => error!("failed to seed pipeline state: channel closed"),
            Err(err) => error!(%err, "failed to seed pipeline state"),
        }
    }
}

fn runtime_block_on<T, F>(runtime: Arc<Runtime>, future: F) -> Result<T, String>
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::spawn(move || runtime.block_on(future))
            .join()
            .map_err(|_| "runtime helper thread panicked".to_string())
    } else {
        Ok(runtime.block_on(future))
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
    provider_statuses: Vec<RuntimeTtsProviderStatus>,
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
                provider_statuses: Vec::new(),
            };
            core.emit_playback_state();
            core.refresh_provider_statuses(None);
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
                voice_override,
                backend,
                backend_label,
                synthesis_parameters,
                provider_statuses,
                artifact_key,
                audio_bytes,
                duration_ms,
                sha256,
            } => {
                self.on_synthesis_ok(
                    message_id,
                    worker_id,
                    attempt,
                    voice_override,
                    backend,
                    backend_label,
                    synthesis_parameters,
                    provider_statuses,
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
                provider_statuses,
            } => {
                debug!(
                    "synthesis failed for {message_id} (attempt {attempt}): {}",
                    error.code
                );
                self.refresh_provider_statuses(Some(provider_statuses));
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
        let skipped_empty = text.trim().is_empty();
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
            if skipped_empty {
                snapshot.lifecycle.transition(
                    MessageState::Skipped(SkippedState {
                        at: OffsetDateTime::now_utc(),
                        reason: SkipReason::Other("Empty message".to_string()),
                        note: Some("transformation_empty".to_string()),
                    }),
                    TransitionCause::Automatic,
                    None,
                    Some("transform_empty".into()),
                );
            } else {
                snapshot.lifecycle.transition(
                    MessageState::Transformed(TransformedState {
                        completed_at: OffsetDateTime::now_utc(),
                        text: text.clone(),
                    }),
                    TransitionCause::Automatic,
                    None,
                    Some("transform_ok".into()),
                );
            }
            snapshot.clone()
        };

        if skipped_empty {
            self.clear_runtime_hints_for(message_id);
            self.persist_and_emit(snapshot);
            return;
        }

        self.persist_and_emit(snapshot);
        self.schedule_synthesis(message_id, 1, Some(text)).await;
    }

    async fn on_synthesis_ok(
        &mut self,
        message_id: u64,
        worker_id: String,
        attempt: u32,
        voice_override: Option<String>,
        backend: SynthBackendId,
        backend_label: String,
        synthesis_parameters: ProviderOptions,
        provider_statuses: Vec<RuntimeTtsProviderStatus>,
        artifact_key: String,
        audio_bytes: Vec<u8>,
        duration_ms: u32,
        sha256: String,
    ) {
        self.refresh_provider_statuses(Some(provider_statuses));

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

            snapshot.header.metadata.insert(
                "tts_backend".to_string(),
                Value::String(backend.as_str().to_string()),
            );
            snapshot.header.metadata.insert(
                "tts_backend_label".to_string(),
                Value::String(backend_label),
            );
            snapshot
                .header
                .metadata
                .insert("tts_attempt".to_string(), Value::from(attempt as u64));
            let parameters_object = synthesis_parameters
                .into_iter()
                .collect::<JsonMap<String, Value>>();
            if parameters_object.is_empty() {
                snapshot.header.metadata.remove("tts_parameters");
            } else {
                snapshot.header.metadata.insert(
                    "tts_parameters".to_string(),
                    Value::Object(parameters_object),
                );
            }
            if let Some(voice_override) = &voice_override {
                snapshot.header.metadata.insert(
                    "tts_voice_override".to_string(),
                    Value::String(voice_override.clone()),
                );
            } else {
                snapshot.header.metadata.remove("tts_voice_override");
            }

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
            let voice_override = load_chatter_voice_override(&self.shared.db, &snapshot.header);
            snapshot
                .header
                .metadata
                .insert("tts_input_text".to_string(), Value::String(text.clone()));

            snapshot
                .lifecycle
                .stage_spans
                .start(PipelineStage::Synthesis, None);
            if should_transition {
                snapshot.lifecycle.transition(
                    MessageState::Synthesizing(SynthesizingState {
                        backend: preferred_synthesis_backend_id(
                            &self.shared.settings,
                            &self.shared.tts_providers,
                        )
                        .map(SynthBackendId::from),
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
                voice_override,
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

    fn refresh_provider_statuses(&mut self, maybe_statuses: Option<Vec<RuntimeTtsProviderStatus>>) {
        let next = maybe_statuses.unwrap_or_else(|| {
            collect_provider_statuses(&self.shared.settings, &self.shared.tts_providers)
        });
        if next == self.provider_statuses {
            return;
        }
        self.provider_statuses = next;
        let _ = self.event_tx.send(PipelineEvent::ProviderStatusesChanged(
            self.provider_statuses.clone(),
        ));
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

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TtsFallbackStrategyConfig {
    #[default]
    Ordered,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize)]
struct TtsProviderConfig {
    id: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(flatten)]
    options: ProviderOptions,
}

#[derive(Clone, Debug, Deserialize)]
struct TtsConfig {
    #[serde(default)]
    providers: Vec<TtsProviderConfig>,
    #[serde(default)]
    fallback_strategy: TtsFallbackStrategyConfig,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            providers: vec![
                TtsProviderConfig {
                    id: SYSTEM_TTS_BACKEND.to_string(),
                    enabled: true,
                    options: ProviderOptions::new(),
                },
                TtsProviderConfig {
                    id: OPENAI_BACKEND.to_string(),
                    enabled: true,
                    options: ProviderOptions::new(),
                },
            ],
            fallback_strategy: TtsFallbackStrategyConfig::Ordered,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TtsFallbackStrategy {
    Ordered,
}

#[derive(Clone, Debug, PartialEq)]
struct TtsProviderPolicy {
    providers: Vec<TtsProviderPolicyEntry>,
    explicit_provider_list: bool,
    fallback_strategy: TtsFallbackStrategy,
}

impl Default for TtsProviderPolicy {
    fn default() -> Self {
        Self {
            providers: vec![
                TtsProviderPolicyEntry {
                    provider_id: SYSTEM_TTS_BACKEND.to_string(),
                    options: ProviderOptions::new(),
                },
                TtsProviderPolicyEntry {
                    provider_id: OPENAI_BACKEND.to_string(),
                    options: ProviderOptions::new(),
                },
            ],
            explicit_provider_list: false,
            fallback_strategy: TtsFallbackStrategy::Ordered,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct TtsProviderPolicyEntry {
    provider_id: String,
    options: ProviderOptions,
}

#[derive(Clone, Debug, PartialEq)]
struct ProviderCandidate {
    provider_id: String,
    options: ProviderOptions,
}

#[derive(Clone, Debug)]
struct SynthesisDispatchResult {
    result: Result<SynthesisDispatchSuccess, StageError>,
    provider_statuses: Vec<RuntimeTtsProviderStatus>,
}

#[derive(Clone, Debug)]
struct SynthesisDispatchSuccess {
    backend: SynthBackendId,
    backend_label: String,
    audio_bytes: Vec<u8>,
    synthesis_parameters: ProviderOptions,
}

#[derive(Clone, Debug, Deserialize)]
struct OfflineTtsStrategyConfig {
    #[serde(default = "default_offline_engine")]
    engine: String,
    #[serde(default)]
    enable_ultra_light: bool,
}

impl Default for OfflineTtsStrategyConfig {
    fn default() -> Self {
        Self {
            engine: default_offline_engine(),
            enable_ultra_light: false,
        }
    }
}

fn default_offline_engine() -> String {
    PARLER_BACKEND.to_string()
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

fn load_tts_provider_policy(settings: &SettingsService) -> TtsProviderPolicy {
    let mut policy = TtsProviderPolicy::default();
    match settings.get("tts") {
        Ok(record) => match serde_json::from_value::<TtsConfig>(record.value) {
            Ok(config) => {
                policy.providers = config
                    .providers
                    .into_iter()
                    .filter(|provider| provider.enabled)
                    .map(|provider| TtsProviderPolicyEntry {
                        provider_id: provider.id,
                        options: provider.options,
                    })
                    .collect();
                policy.explicit_provider_list = true;
                policy.fallback_strategy = match config.fallback_strategy {
                    TtsFallbackStrategyConfig::Ordered | TtsFallbackStrategyConfig::Unknown => {
                        TtsFallbackStrategy::Ordered
                    }
                };
            }
            Err(err) => warn!("invalid tts setting payload: {err}"),
        },
        Err(err) => warn!("failed to load tts setting: {err}"),
    }
    policy
}

fn load_offline_tts_strategy(settings: &SettingsService) -> OfflineTtsStrategyConfig {
    match settings.get("offline_tts_strategy") {
        Ok(record) => serde_json::from_value::<OfflineTtsStrategyConfig>(record.value)
            .unwrap_or_else(|err| {
                warn!("invalid offline_tts_strategy setting payload: {err}");
                OfflineTtsStrategyConfig::default()
            }),
        Err(err) => {
            warn!("failed to load offline_tts_strategy setting: {err}");
            OfflineTtsStrategyConfig::default()
        }
    }
}

fn configured_provider_candidates(
    policy: &TtsProviderPolicy,
    providers: &HashMap<String, Arc<dyn RuntimeTtsProvider>>,
) -> Vec<ProviderCandidate> {
    let mut seen = HashSet::new();
    let mut configured = Vec::new();
    for provider in &policy.providers {
        let normalized = provider.provider_id.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            continue;
        }
        if seen.insert(normalized.clone()) {
            configured.push(ProviderCandidate {
                provider_id: normalized,
                options: provider.options.clone(),
            });
        }
    }

    if !configured.is_empty() {
        return configured;
    }

    // When the setting explicitly lists providers but all are disabled, do not silently
    // re-enable defaults.
    if policy.explicit_provider_list {
        return configured;
    }

    let mut defaults = Vec::new();
    for provider_id in [SYSTEM_TTS_BACKEND, OPENAI_BACKEND] {
        if providers.contains_key(provider_id) {
            defaults.push(ProviderCandidate {
                provider_id: provider_id.to_string(),
                options: ProviderOptions::new(),
            });
        }
    }

    if !defaults.is_empty() {
        return defaults;
    }

    let mut fallback = providers.keys().cloned().collect::<Vec<_>>();
    fallback.sort();
    fallback
        .into_iter()
        .map(|provider_id| ProviderCandidate {
            provider_id,
            options: ProviderOptions::new(),
        })
        .collect()
}

fn preferred_synthesis_backend_id(
    settings: &SettingsService,
    providers: &HashMap<String, Arc<dyn RuntimeTtsProvider>>,
) -> Option<String> {
    let policy = load_tts_provider_policy(settings);
    configured_provider_candidates(&policy, providers)
        .into_iter()
        .map(|candidate| candidate.provider_id)
        .find(|provider_id| providers.contains_key(provider_id))
}

fn collect_provider_statuses(
    settings: &SettingsService,
    providers: &HashMap<String, Arc<dyn RuntimeTtsProvider>>,
) -> Vec<RuntimeTtsProviderStatus> {
    let policy = load_tts_provider_policy(settings);
    let mut statuses = Vec::new();
    for provider in configured_provider_candidates(&policy, providers) {
        push_provider_readiness_status(&mut statuses, settings, providers, &provider.provider_id);
    }
    statuses
}

fn push_provider_readiness_status(
    statuses: &mut Vec<RuntimeTtsProviderStatus>,
    settings: &SettingsService,
    providers: &HashMap<String, Arc<dyn RuntimeTtsProvider>>,
    provider_id: &str,
) {
    let Some(provider) = providers.get(provider_id) else {
        statuses.push(RuntimeTtsProviderStatus {
            provider_id: provider_id.to_string(),
            label: provider_id.to_string(),
            state: RuntimeTtsProviderState::Unavailable,
            detail: Some("Unknown provider".to_string()),
        });
        return;
    };

    if let Some(detail) = provider.readiness_detail(settings) {
        statuses.push(RuntimeTtsProviderStatus {
            provider_id: provider.provider_id().to_string(),
            label: provider.label().to_string(),
            state: RuntimeTtsProviderState::Unavailable,
            detail: Some(detail),
        });
        return;
    }

    statuses.push(RuntimeTtsProviderStatus {
        provider_id: provider.provider_id().to_string(),
        label: provider.label().to_string(),
        state: RuntimeTtsProviderState::Ready,
        detail: None,
    });
}

fn stage_error_summary(error: &StageError) -> String {
    match &error.detail {
        Some(detail) => format!("{} ({detail})", error.code),
        None => error.code.clone(),
    }
}

fn option_string(options: &ProviderOptions, key: &str) -> Option<String> {
    options
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn parse_openai_speech_options(
    options: &ProviderOptions,
) -> (OpenAiSpeechOptions, ProviderOptions) {
    let mut effective = ProviderOptions::new();

    let speech_options = OpenAiSpeechOptions {
        model: option_string(options, "model").unwrap_or_else(|| "gpt-4o-mini-tts".to_string()),
        voice: option_string(options, "voice").unwrap_or_else(|| "alloy".to_string()),
        response_format: option_string(options, "response_format")
            .unwrap_or_else(|| "wav".to_string()),
    };

    effective.insert(
        "model".to_string(),
        Value::String(speech_options.model.clone()),
    );
    effective.insert(
        "voice".to_string(),
        Value::String(speech_options.voice.clone()),
    );
    effective.insert(
        "response_format".to_string(),
        Value::String(speech_options.response_format.clone()),
    );

    (speech_options, effective)
}

fn parse_parler_load_config(options: &ProviderOptions) -> ParlerLoadConfig {
    let mut config = ParlerLoadConfig::default();

    if let Some(repo) = option_string(options, "repo") {
        config.repo = repo;
    }

    if let Some(revision) = option_string(options, "revision") {
        config.revision = Some(revision);
    }

    if let Some(description) = option_string(options, "description") {
        config.default_description = description;
    }

    if let Some(device) = option_string(options, "device") {
        config.device = match device.to_ascii_lowercase().as_str() {
            "cpu" => lupo_tts_parler::DevicePreference::Cpu,
            "metal" => lupo_tts_parler::DevicePreference::Metal,
            _ => lupo_tts_parler::DevicePreference::Auto,
        };
    }

    config
}

fn parler_parameters(config: &ParlerLoadConfig) -> ProviderOptions {
    let mut parameters = ProviderOptions::new();
    parameters.insert("repo".to_string(), Value::String(config.repo.clone()));
    if let Some(revision) = &config.revision {
        parameters.insert("revision".to_string(), Value::String(revision.clone()));
    }
    parameters.insert(
        "device".to_string(),
        Value::String(
            match config.device {
                lupo_tts_parler::DevicePreference::Auto => "auto",
                lupo_tts_parler::DevicePreference::Cpu => "cpu",
                lupo_tts_parler::DevicePreference::Metal => "metal",
            }
            .to_string(),
        ),
    );
    parameters
}

fn synthesize_ultra_light_wav(text: &str) -> Result<Vec<u8>, hound::Error> {
    let sample_rate = 16_000_u32;
    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };

    let mut cursor = Cursor::new(Vec::<u8>::new());
    let mut writer = WavWriter::new(&mut cursor, spec)?;

    let mut wrote_any = false;
    for ch in text.chars().take(320) {
        if ch.is_whitespace() {
            let silence_samples = sample_rate.saturating_mul(35) / 1_000;
            for _ in 0..silence_samples {
                writer.write_sample(0_i16)?;
            }
            continue;
        }

        let base_hz = 420.0_f32;
        let offset = (u32::from(ch) % 28) as f32 * 11.5;
        let freq_hz = base_hz + offset;
        let tone_ms = if ch.is_ascii_punctuation() { 42 } else { 58 };
        let sample_count = sample_rate.saturating_mul(tone_ms) / 1_000;
        for index in 0..sample_count {
            let phase =
                2.0_f32 * std::f32::consts::PI * freq_hz * index as f32 / sample_rate as f32;
            let sample = (phase.sin() * 0.18_f32 * i16::MAX as f32) as i16;
            writer.write_sample(sample)?;
        }
        let silence_samples = sample_rate.saturating_mul(14) / 1_000;
        for _ in 0..silence_samples {
            writer.write_sample(0_i16)?;
        }
        wrote_any = true;
    }

    if !wrote_any {
        let silence_samples = sample_rate.saturating_mul(120) / 1_000;
        for _ in 0..silence_samples {
            writer.write_sample(0_i16)?;
        }
    }

    writer.finalize()?;
    Ok(cursor.into_inner())
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

fn load_chatter_voice_override(
    db: &lupo_persistence::DatabaseHandle,
    header: &MessageHeader,
) -> Option<String> {
    let chatter_id =
        canonical_chatter_id(&header.provider, header.user_id.as_deref(), &header.sender);
    let record = match load_chatter_override(db, &chatter_id) {
        Ok(value) => value,
        Err(err) => {
            warn!(%chatter_id, "failed to load chatter override: {err}");
            None
        }
    }?;

    record
        .voice_override
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn merge_voice_override(
    options: &ProviderOptions,
    voice_override: Option<&str>,
) -> ProviderOptions {
    let mut merged = options.clone();
    if let Some(voice_override) = voice_override {
        let trimmed = voice_override.trim();
        if !trimmed.is_empty() {
            merged.insert("voice".to_string(), Value::String(trimmed.to_string()));
        }
    }
    merged
}

async fn synthesize_with_fallback(
    message_id: Option<u64>,
    settings: &SettingsService,
    providers: &HashMap<String, Arc<dyn RuntimeTtsProvider>>,
    text: &str,
    voice_override: Option<&str>,
) -> SynthesisDispatchResult {
    let policy = load_tts_provider_policy(settings);
    match policy.fallback_strategy {
        TtsFallbackStrategy::Ordered => {}
    }

    let configured = configured_provider_candidates(&policy, providers);
    let mut provider_statuses = Vec::new();
    let mut attempted = false;
    let mut any_retryable = false;
    let mut failures = Vec::new();

    for (index, configured_provider) in configured.iter().enumerate() {
        let provider_id = &configured_provider.provider_id;
        let remaining_candidates = configured.len().saturating_sub(index + 1);
        let Some(provider) = providers.get(provider_id) else {
            if remaining_candidates > 0 {
                warn!(
                    message_id = ?message_id,
                    failed_provider = %provider_id,
                    remaining_candidates,
                    "unknown TTS provider in policy; falling back to next provider"
                );
            }
            provider_statuses.push(RuntimeTtsProviderStatus {
                provider_id: provider_id.clone(),
                label: provider_id.clone(),
                state: RuntimeTtsProviderState::Unavailable,
                detail: Some("Unknown provider".to_string()),
            });
            continue;
        };

        if let Some(detail) = provider.readiness_detail(settings) {
            if remaining_candidates > 0 {
                warn!(
                    message_id = ?message_id,
                    failed_provider = %provider.provider_id(),
                    provider_label = %provider.label(),
                    reason = %detail,
                    remaining_candidates,
                    "TTS provider unavailable; falling back to next provider"
                );
            }
            provider_statuses.push(RuntimeTtsProviderStatus {
                provider_id: provider.provider_id().to_string(),
                label: provider.label().to_string(),
                state: RuntimeTtsProviderState::Unavailable,
                detail: Some(detail),
            });
            continue;
        }

        attempted = true;
        let effective_options = merge_voice_override(&configured_provider.options, voice_override);
        match provider
            .synthesize(settings.clone(), text.to_string(), effective_options)
            .await
        {
            Ok(synthesis_result) => {
                if index > 0 {
                    info!(
                        message_id = ?message_id,
                        selected_provider = %provider.provider_id(),
                        selected_label = %provider.label(),
                        provider_rank = index + 1,
                        candidate_count = configured.len(),
                        "TTS synthesis fallback succeeded with alternate provider"
                    );
                }
                provider_statuses.push(RuntimeTtsProviderStatus {
                    provider_id: provider.provider_id().to_string(),
                    label: provider.label().to_string(),
                    state: RuntimeTtsProviderState::Ready,
                    detail: None,
                });
                for pending in configured.iter().skip(index + 1) {
                    push_provider_readiness_status(
                        &mut provider_statuses,
                        settings,
                        providers,
                        &pending.provider_id,
                    );
                }
                return SynthesisDispatchResult {
                    result: Ok(SynthesisDispatchSuccess {
                        backend: SynthBackendId::from(provider.provider_id()),
                        backend_label: provider.label().to_string(),
                        audio_bytes: synthesis_result.audio_bytes,
                        synthesis_parameters: synthesis_result.parameters,
                    }),
                    provider_statuses,
                };
            }
            Err(error) => {
                let failure_summary = stage_error_summary(&error);
                if remaining_candidates > 0 {
                    warn!(
                        message_id = ?message_id,
                        failed_provider = %provider.provider_id(),
                        provider_label = %provider.label(),
                        error_code = %error.code,
                        error_detail = ?error.detail,
                        retryable = error.retryable,
                        remaining_candidates,
                        "TTS provider failed; falling back to next provider"
                    );
                } else {
                    warn!(
                        message_id = ?message_id,
                        failed_provider = %provider.provider_id(),
                        provider_label = %provider.label(),
                        error_code = %error.code,
                        error_detail = ?error.detail,
                        retryable = error.retryable,
                        "TTS provider failed and no fallback providers remain"
                    );
                }
                any_retryable |= error.retryable;
                failures.push(format!("{provider_id}: {failure_summary}"));
                provider_statuses.push(RuntimeTtsProviderStatus {
                    provider_id: provider.provider_id().to_string(),
                    label: provider.label().to_string(),
                    state: RuntimeTtsProviderState::Error,
                    detail: Some(failure_summary),
                });
            }
        }
    }

    let error = if attempted {
        warn!(
            message_id = ?message_id,
            failure_count = failures.len(),
            failures = %failures.join(" | "),
            "all configured TTS providers failed"
        );
        StageError {
            code: "tts_all_providers_failed".to_string(),
            detail: Some(failures.join(" | ")),
            retryable: any_retryable,
        }
    } else {
        let detail = if provider_statuses.is_empty() {
            "No enabled providers in tts.providers".to_string()
        } else {
            provider_statuses
                .iter()
                .filter_map(|status| match status.state {
                    RuntimeTtsProviderState::Unavailable => Some(match &status.detail {
                        Some(detail) => format!("{}: {detail}", status.provider_id),
                        None => status.provider_id.clone(),
                    }),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" | ")
        };
        warn!(
            message_id = ?message_id,
            detail = %detail,
            "no TTS provider available for synthesis"
        );
        StageError::non_retryable("no_tts_provider_available", Some(detail))
    };

    SynthesisDispatchResult {
        result: Err(error),
        provider_statuses,
    }
}

async fn process_synthesis_job(
    shared: &PipelineShared,
    worker_id: &str,
    job: SynthesisJob,
) -> StageResult {
    let synthesis = synthesize_with_fallback(
        Some(job.message_id),
        &shared.settings,
        &shared.tts_providers,
        &job.text,
        job.voice_override.as_deref(),
    )
    .await;
    let success = match synthesis.result {
        Ok(result) => result,
        Err(error) => {
            return StageResult::SynthesisErr {
                message_id: job.message_id,
                worker_id: worker_id.to_string(),
                attempt: job.attempt,
                error,
                provider_statuses: synthesis.provider_statuses,
            };
        }
    };

    let duration_ms = wav_duration_ms(success.audio_bytes.as_slice()).unwrap_or(0);
    let sha256 = sha256_hex(success.audio_bytes.as_slice());
    let artifact_key = artifact_storage_key(job.message_id, job.attempt);

    StageResult::SynthesisOk {
        message_id: job.message_id,
        worker_id: worker_id.to_string(),
        attempt: job.attempt,
        voice_override: job.voice_override,
        backend: success.backend,
        backend_label: success.backend_label,
        synthesis_parameters: success.synthesis_parameters,
        provider_statuses: synthesis.provider_statuses,
        artifact_key,
        audio_bytes: success.audio_bytes,
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
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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
    use lupo_persistence::open_database_at_path;
    use serde_json::json;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::runtime::Runtime;

    type ProviderOptionsLog = Arc<std::sync::Mutex<Vec<ProviderOptions>>>;
    type FakeProviderWithOptions = (
        Arc<dyn RuntimeTtsProvider>,
        Arc<AtomicUsize>,
        ProviderOptionsLog,
    );

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

    fn test_settings_service() -> (SettingsService, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let runtime = Runtime::new().expect("failed to create runtime");
        let db = runtime
            .block_on(async { open_database_at_path(temp_dir.path()).await })
            .expect("failed to open test database");
        (SettingsService::new(db), temp_dir)
    }

    struct FakeRuntimeTtsProvider {
        id: &'static str,
        label: &'static str,
        readiness_detail: Option<String>,
        synth_result: Result<ProviderSynthesisResult, StageError>,
        call_count: Arc<AtomicUsize>,
        seen_options: Option<Arc<std::sync::Mutex<Vec<ProviderOptions>>>>,
    }

    impl RuntimeTtsProvider for FakeRuntimeTtsProvider {
        fn provider_id(&self) -> &'static str {
            self.id
        }

        fn label(&self) -> &'static str {
            self.label
        }

        fn readiness_detail(&self, _settings: &SettingsService) -> Option<String> {
            self.readiness_detail.clone()
        }

        fn synthesize(
            &self,
            _settings: SettingsService,
            _text: String,
            options: ProviderOptions,
        ) -> SynthesisFuture {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            if let Some(seen_options) = &self.seen_options
                && let Ok(mut guard) = seen_options.lock()
            {
                guard.push(options.clone());
            }
            let result = self.synth_result.clone();
            Box::pin(async move { result })
        }
    }

    fn fake_provider(
        id: &'static str,
        label: &'static str,
        readiness_detail: Option<String>,
        synth_result: Result<ProviderSynthesisResult, StageError>,
    ) -> (Arc<dyn RuntimeTtsProvider>, Arc<AtomicUsize>) {
        let call_count = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn RuntimeTtsProvider> = Arc::new(FakeRuntimeTtsProvider {
            id,
            label,
            readiness_detail,
            synth_result,
            call_count: call_count.clone(),
            seen_options: None,
        });
        (provider, call_count)
    }

    fn fake_provider_with_options(
        id: &'static str,
        label: &'static str,
        readiness_detail: Option<String>,
        synth_result: Result<ProviderSynthesisResult, StageError>,
    ) -> FakeProviderWithOptions {
        let call_count = Arc::new(AtomicUsize::new(0));
        let seen_options = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn RuntimeTtsProvider> = Arc::new(FakeRuntimeTtsProvider {
            id,
            label,
            readiness_detail,
            synth_result,
            call_count: call_count.clone(),
            seen_options: Some(seen_options.clone()),
        });
        (provider, call_count, seen_options)
    }

    fn provider_map(
        providers: Vec<Arc<dyn RuntimeTtsProvider>>,
    ) -> HashMap<String, Arc<dyn RuntimeTtsProvider>> {
        let mut map = HashMap::new();
        for provider in providers {
            map.insert(provider.provider_id().to_string(), provider);
        }
        map
    }

    #[test]
    fn parse_command_respects_prefix_boundary() {
        let command = parse_command("!lupo skip 42", "!lupo").expect("expected command");
        assert_eq!(command.command, "skip");
        assert_eq!(command.args, vec!["42".to_string()]);

        assert!(parse_command("!lupobot", "!lupo").is_none());
    }

    #[test]
    fn normalize_for_tts_collapses_whitespace() {
        assert_eq!(normalize_for_tts(" hello \n  world\t"), "hello world");
    }

    #[test]
    fn normalize_for_tts_leaves_blank_content_empty() {
        assert_eq!(normalize_for_tts(" \n\t "), "");
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

    #[test]
    fn runtime_block_on_works_inside_existing_runtime() {
        let outer_runtime = Runtime::new().expect("failed to create outer runtime");
        outer_runtime.block_on(async {
            let inner_runtime = Arc::new(Runtime::new().expect("failed to create inner runtime"));
            let value =
                runtime_block_on(inner_runtime, async { 42_u32 }).expect("runtime_block_on failed");
            assert_eq!(value, 42);
        });
    }

    #[test]
    fn load_filter_policy_reflects_live_settings_changes() {
        let (settings, _temp_dir) = test_settings_service();

        let defaults = load_filter_policy(&settings);
        assert_eq!(defaults.rules.ignore_prefix, "!");
        assert_eq!(defaults.rules.command_prefix, "!lupo");
        assert!(defaults.rules.suppress_provider_bots);
        assert!(!defaults.rules.ignore_paid);
        assert_eq!(defaults.command_acceptance, CommandAcceptance::Moderators);

        settings
            .set(
                "message_filtering",
                json!({
                    "mode": "blacklist",
                    "rules": {
                        "ignore_prefix": "#",
                        "command_prefix": "$lupo",
                        "suppress_provider_bots": false,
                        "ignore_paid": true
                    }
                }),
            )
            .expect("failed to update message_filtering setting");
        settings
            .set("command_acceptance", json!("channel_owner"))
            .expect("failed to update command_acceptance setting");

        let updated = load_filter_policy(&settings);
        assert_eq!(updated.rules.ignore_prefix, "#");
        assert_eq!(updated.rules.command_prefix, "$lupo");
        assert!(!updated.rules.suppress_provider_bots);
        assert!(updated.rules.ignore_paid);
        assert_eq!(updated.command_acceptance, CommandAcceptance::ChannelOwner);
    }

    #[test]
    fn load_max_predicted_queue_duration_reads_latest_advanced_setting() {
        let (settings, _temp_dir) = test_settings_service();

        let baseline = load_max_predicted_queue_duration_ms(&settings);
        assert_eq!(
            baseline,
            default_max_predicted_queue_duration_secs().saturating_mul(1000)
        );

        settings
            .set(
                "advanced",
                json!({
                    "pipeline": {
                        "arrived_queue": 128,
                        "filtered_queue": 128,
                        "synthesis_queue": 32
                    },
                    "scheduler": {
                        "max_parallel_synthesis": 4,
                        "max_predicted_queue_duration_secs": 42
                    },
                    "feature_flags": {
                        "enable_latency_probe": false
                    }
                }),
            )
            .expect("failed to update advanced setting");

        let updated = load_max_predicted_queue_duration_ms(&settings);
        assert_eq!(updated, 42_000);
    }

    #[test]
    fn default_provider_policy_falls_back_from_unavailable_system_to_openai() {
        let (settings, _temp_dir) = test_settings_service();
        let (openai, openai_calls) = fake_provider(
            OPENAI_BACKEND,
            "OpenAI",
            None,
            Ok(ProviderSynthesisResult {
                audio_bytes: vec![1, 2, 3],
                parameters: ProviderOptions::new(),
            }),
        );
        let providers = provider_map(vec![openai]);
        let runtime = Runtime::new().expect("failed to create runtime");

        let result = runtime.block_on(async {
            synthesize_with_fallback(None, &settings, &providers, "hello world", None).await
        });
        let success = result.result.expect("expected fallback synthesis success");
        assert_eq!(success.backend.as_str(), OPENAI_BACKEND);
        assert_eq!(success.audio_bytes, vec![1, 2, 3]);
        assert_eq!(openai_calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.provider_statuses.len(), 2);
        assert_eq!(
            result.provider_statuses[0].provider_id,
            SYSTEM_TTS_BACKEND.to_string()
        );
        assert!(matches!(
            result.provider_statuses[0].state,
            RuntimeTtsProviderState::Unavailable
        ));
    }

    #[test]
    fn synthesis_ignores_disabled_provider_and_unknown_provider_deterministically() {
        let (settings, _temp_dir) = test_settings_service();
        settings
            .set(
                "tts",
                json!({
                    "providers": [
                        {"id": "unknown_provider", "enabled": true},
                        {"id": OPENAI_BACKEND, "enabled": false},
                        {"id": SYSTEM_TTS_BACKEND, "enabled": true}
                    ],
                    "fallback_strategy": "ordered",
                    "retry_limit": 3
                }),
            )
            .expect("failed to update tts setting");
        let (system, system_calls) = fake_provider(
            SYSTEM_TTS_BACKEND,
            "System TTS",
            None,
            Ok(ProviderSynthesisResult {
                audio_bytes: vec![7, 7],
                parameters: ProviderOptions::new(),
            }),
        );
        let providers = provider_map(vec![system]);
        let runtime = Runtime::new().expect("failed to create runtime");

        let result = runtime.block_on(async {
            synthesize_with_fallback(None, &settings, &providers, "hello world", None).await
        });
        let success = result.result.expect("expected synthesis success");
        assert_eq!(success.backend.as_str(), SYSTEM_TTS_BACKEND);
        assert_eq!(system_calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.provider_statuses.len(), 2);
        assert_eq!(result.provider_statuses[0].provider_id, "unknown_provider");
        assert_eq!(result.provider_statuses[1].provider_id, SYSTEM_TTS_BACKEND);
    }

    #[test]
    fn synthesis_falls_back_to_next_provider_after_failure() {
        let (settings, _temp_dir) = test_settings_service();
        settings
            .set(
                "tts",
                json!({
                    "providers": [
                        {"id": SYSTEM_TTS_BACKEND, "enabled": true},
                        {"id": OPENAI_BACKEND, "enabled": true}
                    ],
                    "fallback_strategy": "ordered",
                    "retry_limit": 3
                }),
            )
            .expect("failed to update tts setting");
        let (system, _) = fake_provider(
            SYSTEM_TTS_BACKEND,
            "System TTS",
            None,
            Err(StageError::retryable(
                "system_failed",
                Some("boom".to_string()),
            )),
        );
        let (openai, openai_calls) = fake_provider(
            OPENAI_BACKEND,
            "OpenAI",
            None,
            Ok(ProviderSynthesisResult {
                audio_bytes: vec![9, 9, 9],
                parameters: ProviderOptions::new(),
            }),
        );
        let providers = provider_map(vec![system, openai]);
        let runtime = Runtime::new().expect("failed to create runtime");

        let result = runtime.block_on(async {
            synthesize_with_fallback(None, &settings, &providers, "hello world", None).await
        });
        let success = result.result.expect("expected fallback synthesis success");
        assert_eq!(success.backend.as_str(), OPENAI_BACKEND);
        assert_eq!(openai_calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            result.provider_statuses[0].state,
            RuntimeTtsProviderState::Error
        ));
        assert!(matches!(
            result.provider_statuses[1].state,
            RuntimeTtsProviderState::Ready
        ));
    }

    #[test]
    fn synthesis_reports_all_providers_failed_when_every_ready_provider_errors() {
        let (settings, _temp_dir) = test_settings_service();
        settings
            .set(
                "tts",
                json!({
                    "providers": [
                        {"id": SYSTEM_TTS_BACKEND, "enabled": true},
                        {"id": OPENAI_BACKEND, "enabled": true}
                    ],
                    "fallback_strategy": "ordered",
                    "retry_limit": 3
                }),
            )
            .expect("failed to update tts setting");
        let (system, _) = fake_provider(
            SYSTEM_TTS_BACKEND,
            "System TTS",
            None,
            Err(StageError::non_retryable(
                "system_failed",
                Some("system down".to_string()),
            )),
        );
        let (openai, _) = fake_provider(
            OPENAI_BACKEND,
            "OpenAI",
            None,
            Err(StageError::retryable(
                "openai_failed",
                Some("rate_limited".to_string()),
            )),
        );
        let providers = provider_map(vec![system, openai]);
        let runtime = Runtime::new().expect("failed to create runtime");

        let result = runtime.block_on(async {
            synthesize_with_fallback(None, &settings, &providers, "hello world", None).await
        });
        let error = result.result.expect_err("expected synthesis failure");
        assert_eq!(error.code, "tts_all_providers_failed");
        assert!(error.retryable);
        let detail = error.detail.unwrap_or_default();
        assert!(detail.contains(SYSTEM_TTS_BACKEND));
        assert!(detail.contains(OPENAI_BACKEND));
    }

    #[test]
    fn synthesis_reports_no_available_provider_when_none_can_run() {
        let (settings, _temp_dir) = test_settings_service();
        settings
            .set(
                "tts",
                json!({
                    "providers": [
                        {"id": "unknown_provider", "enabled": true}
                    ],
                    "fallback_strategy": "ordered",
                    "retry_limit": 3
                }),
            )
            .expect("failed to update tts setting");
        let providers = provider_map(Vec::<Arc<dyn RuntimeTtsProvider>>::new());
        let runtime = Runtime::new().expect("failed to create runtime");

        let result = runtime.block_on(async {
            synthesize_with_fallback(None, &settings, &providers, "hello world", None).await
        });
        let error = result.result.expect_err("expected synthesis failure");
        assert_eq!(error.code, "no_tts_provider_available");
        assert!(!error.retryable);
        assert!(matches!(
            result.provider_statuses[0].state,
            RuntimeTtsProviderState::Unavailable
        ));
    }

    #[test]
    fn load_tts_provider_policy_defaults_unknown_fallback_strategy_to_ordered() {
        let (settings, _temp_dir) = test_settings_service();
        settings
            .set(
                "tts",
                json!({
                    "providers": [
                        {"id": OPENAI_BACKEND, "enabled": true}
                    ],
                    "fallback_strategy": "unsupported"
                }),
            )
            .expect("failed to update tts setting");

        let policy = load_tts_provider_policy(&settings);
        assert_eq!(policy.fallback_strategy, TtsFallbackStrategy::Ordered);
    }

    #[test]
    fn load_tts_provider_policy_preserves_enabled_provider_options() {
        let (settings, _temp_dir) = test_settings_service();
        settings
            .set(
                "tts",
                json!({
                    "providers": [
                        {
                            "id": OPENAI_BACKEND,
                            "enabled": true,
                            "model": "gpt-4o-tts",
                            "voice": "nova"
                        },
                        {
                            "id": SYSTEM_TTS_BACKEND,
                            "enabled": false
                        }
                    ],
                    "fallback_strategy": "ordered"
                }),
            )
            .expect("failed to update tts setting");

        let policy = load_tts_provider_policy(&settings);
        assert!(policy.explicit_provider_list);
        assert_eq!(policy.providers.len(), 1);
        assert_eq!(policy.providers[0].provider_id, OPENAI_BACKEND);
        assert_eq!(
            policy.providers[0]
                .options
                .get("model")
                .and_then(Value::as_str),
            Some("gpt-4o-tts")
        );
        assert_eq!(
            policy.providers[0]
                .options
                .get("voice")
                .and_then(Value::as_str),
            Some("nova")
        );
    }

    #[test]
    fn synthesis_respects_explicitly_disabled_all_providers() {
        let (settings, _temp_dir) = test_settings_service();
        settings
            .set(
                "tts",
                json!({
                    "providers": [
                        {"id": SYSTEM_TTS_BACKEND, "enabled": false},
                        {"id": OPENAI_BACKEND, "enabled": false}
                    ],
                    "fallback_strategy": "ordered"
                }),
            )
            .expect("failed to update tts setting");

        let (openai, openai_calls) = fake_provider(
            OPENAI_BACKEND,
            "OpenAI",
            None,
            Ok(ProviderSynthesisResult {
                audio_bytes: vec![1, 2, 3],
                parameters: ProviderOptions::new(),
            }),
        );
        let providers = provider_map(vec![openai]);
        let runtime = Runtime::new().expect("failed to create runtime");

        let result = runtime.block_on(async {
            synthesize_with_fallback(None, &settings, &providers, "hello world", None).await
        });
        let error = result.result.expect_err("expected synthesis failure");
        assert_eq!(error.code, "no_tts_provider_available");
        assert_eq!(openai_calls.load(Ordering::SeqCst), 0);
        assert!(result.provider_statuses.is_empty());
    }

    #[test]
    fn parse_openai_speech_options_applies_overrides() {
        let mut options = ProviderOptions::new();
        options.insert("model".to_string(), Value::String("gpt-4o-tts".to_string()));
        options.insert("voice".to_string(), Value::String("nova".to_string()));
        options.insert(
            "response_format".to_string(),
            Value::String("wav".to_string()),
        );

        let (speech_options, effective) = parse_openai_speech_options(&options);
        assert_eq!(speech_options.model, "gpt-4o-tts");
        assert_eq!(speech_options.voice, "nova");
        assert_eq!(speech_options.response_format, "wav");
        assert_eq!(effective.get("voice").and_then(Value::as_str), Some("nova"));
    }

    #[test]
    fn merge_voice_override_overrides_provider_voice() {
        let mut options = ProviderOptions::new();
        options.insert("voice".to_string(), Value::String("alloy".to_string()));
        options.insert(
            "model".to_string(),
            Value::String("gpt-4o-mini-tts".to_string()),
        );

        let merged = merge_voice_override(&options, Some("nova"));
        assert_eq!(merged.get("voice").and_then(Value::as_str), Some("nova"));
        assert_eq!(
            merged.get("model").and_then(Value::as_str),
            Some("gpt-4o-mini-tts")
        );
    }

    #[test]
    fn merge_voice_override_keeps_configured_voice_when_absent() {
        let mut options = ProviderOptions::new();
        options.insert("voice".to_string(), Value::String("alloy".to_string()));

        let merged = merge_voice_override(&options, None);
        assert_eq!(merged.get("voice").and_then(Value::as_str), Some("alloy"));
    }

    #[test]
    fn synthesize_with_fallback_passes_voice_override_option() {
        let (settings, _temp_dir) = test_settings_service();
        settings
            .set(
                "tts",
                json!({
                    "providers": [
                        {"id": OPENAI_BACKEND, "enabled": true}
                    ],
                    "fallback_strategy": "ordered"
                }),
            )
            .expect("failed to update tts setting");

        let (provider, calls, seen_options) = fake_provider_with_options(
            OPENAI_BACKEND,
            "OpenAI",
            None,
            Ok(ProviderSynthesisResult {
                audio_bytes: vec![1, 2, 3],
                parameters: ProviderOptions::new(),
            }),
        );
        let providers = provider_map(vec![provider]);
        let runtime = Runtime::new().expect("failed to create runtime");

        let result = runtime.block_on(async {
            synthesize_with_fallback(None, &settings, &providers, "hello world", Some("sol")).await
        });
        assert!(result.result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let seen = seen_options.lock().expect("lock seen options");
        let first = seen.first().expect("expected one options payload");
        assert_eq!(first.get("voice").and_then(Value::as_str), Some("sol"));
    }

    #[test]
    fn synthesize_ultra_light_wav_produces_valid_audio() {
        let wav_bytes =
            synthesize_ultra_light_wav("hello world").expect("ultra-light synthesis should work");

        let mut reader =
            WavReader::new(Cursor::new(wav_bytes)).expect("result should be a valid wav payload");
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, SampleFormat::Int);
        assert!(reader.samples::<i16>().next().is_some());
    }
}

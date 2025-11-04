// SPDX-License-Identifier: Apache-2.0

//! Shared persistence helpers for LUPO applications backed by `redb`.

use directories::ProjectDirs;
use redb::{
    Database, ReadTransaction, ReadableDatabase, ReadableTable, TableDefinition, WriteTransaction,
};
use serde::{Deserialize, Serialize, de::Error as SerdeDeError};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{env, fs::create_dir_all};
use thiserror::Error;
use time::OffsetDateTime;

/// Application qualifier used when resolving OS-specific directories.
pub const APP_QUALIFIER: &str = "dev";
/// Application organization/company identifier.
pub const APP_ORGANIZATION: &str = "sugoijan";
/// Application name exposed to the OS when resolving directories.
pub const APP_APPLICATION: &str = "LUPO TTS";
/// `redb` filename stored under the data directory.
pub const DATABASE_FILENAME: &str = "lupo.redb";

/// Definition for the persisted configuration table.
pub const SETTINGS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("settings");
/// Definition for the secret key/value table.
pub const SECRETS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("secrets");
/// Definition for the queued messages table.
pub const MESSAGES_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("messages");
/// Definition for message-associated binary artifacts (e.g., synthesized audio).
pub const MESSAGE_ARTIFACTS_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("message_artifacts");
/// Definition for runtime/session state blobs managed by application services.
pub const APP_STATE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("app_state");
/// Definition for persisted chatter override records.
pub const CHATTER_OVERRIDES_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("chatter_overrides");
/// Definition for internal metadata such as auto-increment counters.
pub const METADATA_TABLE: TableDefinition<&str, u64> = TableDefinition::new("__meta");

const MESSAGES_NEXT_ID_KEY: &str = "messages_next_id";

/// Convenience alias for the shared database handle.
pub type DatabaseHandle = Arc<Database>;

/// Errors emitted by the persistence helpers.
#[derive(Debug, Error)]
pub enum Error {
    /// Unable to look up the platform-specific project directories.
    #[error("project directories unavailable")]
    MissingProjectDirectories,
    /// Filesystem interactions failed (e.g., creating the data directory).
    #[error("filesystem error")]
    Io(#[from] std::io::Error),
    /// `redb` failures (open, read, write, commit, etc.).
    #[error("storage error: {0}")]
    Storage(String),
    /// JSON (de)serialisation issues while reading or writing payloads.
    #[error("serialization error")]
    Serialization(#[from] serde_json::Error),
}

impl From<redb::Error> for Error {
    fn from(err: redb::Error) -> Self {
        Error::Storage(err.to_string())
    }
}

impl From<redb::TableError> for Error {
    fn from(err: redb::TableError) -> Self {
        Error::Storage(err.to_string())
    }
}

impl From<redb::CommitError> for Error {
    fn from(err: redb::CommitError) -> Self {
        Error::Storage(err.to_string())
    }
}

impl From<redb::DatabaseError> for Error {
    fn from(err: redb::DatabaseError) -> Self {
        Error::Storage(err.to_string())
    }
}

impl From<redb::TransactionError> for Error {
    fn from(err: redb::TransactionError) -> Self {
        Error::Storage(err.to_string())
    }
}

impl From<redb::StorageError> for Error {
    fn from(err: redb::StorageError) -> Self {
        Error::Storage(err.to_string())
    }
}

/// Return the canonical [`ProjectDirs`] for the application.
pub fn app_project_dirs() -> Result<ProjectDirs, Error> {
    if let Ok(dir) = env::var("LUPO_DATA_DIR") {
        let path = PathBuf::from(dir);
        ProjectDirs::from_path(path).ok_or(Error::MissingProjectDirectories)
    } else {
        ProjectDirs::from(APP_QUALIFIER, APP_ORGANIZATION, APP_APPLICATION)
            .ok_or(Error::MissingProjectDirectories)
    }
}

/// Ensure the on-disk data directory exists before connecting.
fn ensure_data_dir(dir: impl AsRef<Path>) -> Result<(), Error> {
    let dir = dir.as_ref();
    if !dir.exists() {
        create_dir_all(dir)?;
    }
    Ok(())
}

/// Open the shared application database rooted at `data_dir`.
pub async fn open_database_at_path(data_dir: impl AsRef<Path>) -> Result<DatabaseHandle, Error> {
    ensure_data_dir(&data_dir)?;
    let path = data_dir.as_ref().join(DATABASE_FILENAME);
    let db = if path.exists() {
        Database::open(path)?
    } else {
        Database::create(path)?
    };
    let handle = Arc::new(db);
    initialise_schema(&handle)?;
    Ok(handle)
}

/// Open the database using the standard application directories.
pub async fn open_database_with_dirs(dirs: &ProjectDirs) -> Result<DatabaseHandle, Error> {
    open_database_at_path(dirs.data_local_dir()).await
}

/// Resolve the standard directories and open the application database.
pub async fn open_default_database() -> Result<(ProjectDirs, DatabaseHandle), Error> {
    let dirs = app_project_dirs()?;
    let db = open_database_with_dirs(&dirs).await?;
    Ok((dirs, db))
}

fn initialise_schema(db: &DatabaseHandle) -> Result<(), Error> {
    with_write_txn(db, |txn| {
        // Ensure all tables exist.
        txn.open_table(SETTINGS_TABLE)?;
        txn.open_table(SECRETS_TABLE)?;
        txn.open_table(MESSAGES_TABLE)?;
        txn.open_table(MESSAGE_ARTIFACTS_TABLE)?;
        txn.open_table(APP_STATE_TABLE)?;
        txn.open_table(CHATTER_OVERRIDES_TABLE)?;

        {
            let mut meta = txn.open_table(METADATA_TABLE)?;
            if meta.get(MESSAGES_NEXT_ID_KEY)?.is_none() {
                meta.insert(MESSAGES_NEXT_ID_KEY, 1)?;
            }
        }

        Ok(())
    })
}

const CURRENT_MESSAGE_VERSION: u16 = 2;

/// Complete snapshot of a message, its lifecycle, and related artifacts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageSnapshot {
    pub header: MessageHeader,
    pub lifecycle: Lifecycle,
    pub artifacts: ArtifactSet,
}

impl MessageSnapshot {
    /// Shorthand for retrieving the current state kind.
    pub fn state_kind(&self) -> MessageStateKind {
        self.lifecycle.state.kind()
    }
}

/// Immutable header fields that describe a message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageHeader {
    pub id: u64,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub sender: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_timestamp: Option<OffsetDateTime>,
    pub received_at: OffsetDateTime,
    pub delivery_kind: DeliveryKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment: Option<PaymentInfo>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, JsonValue>,
}

/// Per-chatter persisted override values used by the desktop shell and synthesis pipeline.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatterOverrideRecord {
    pub chatter_id: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice_override: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name_override: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub updated_at: OffsetDateTime,
}

/// Build a canonical chatter identifier shared by runtime and UI logic.
pub fn canonical_chatter_id(provider: &str, user_id: Option<&str>, sender: &str) -> String {
    let provider_norm = provider.trim().to_ascii_lowercase();
    if let Some(user_id) = user_id {
        let user_norm = user_id.trim();
        if !user_norm.is_empty() {
            return format!("{provider_norm}:user:{}", user_norm.to_ascii_lowercase());
        }
    }

    let sender_norm = sender
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    format!("{provider_norm}:sender:{sender_norm}")
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryKind {
    #[default]
    Standard,
    Paid,
    Other,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaymentInfo {
    pub amount_minor_units: u64,
    pub currency_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_tier_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_color_rank: Option<String>,
}

/// Mutable lifecycle data associated with a message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lifecycle {
    pub state: MessageState,
    #[serde(default)]
    pub stage_spans: StageSpans,
    #[serde(default)]
    pub history: Vec<TransitionLog>,
    #[serde(default)]
    pub next_seq: u64,
}

impl Lifecycle {
    /// Create a lifecycle rooted at the provided state and seed the history log.
    pub fn new(initial_state: MessageState) -> Self {
        let at = OffsetDateTime::now_utc();
        let history = vec![TransitionLog {
            seq: 0,
            at,
            from: None,
            to: initial_state.kind(),
            cause: TransitionCause::System,
            worker: None,
            note: Some("message_created".into()),
        }];

        Self {
            state: initial_state,
            stage_spans: StageSpans::default(),
            history,
            next_seq: 1,
        }
    }

    /// Append a lifecycle transition and update the active state.
    pub fn transition(
        &mut self,
        to: MessageState,
        cause: TransitionCause,
        worker: Option<String>,
        note: Option<String>,
    ) {
        let from_kind = Some(self.state.kind());
        let to_kind = to.kind();
        let seq = self.next_seq;
        self.next_seq += 1;
        self.state = to;
        self.history.push(TransitionLog {
            seq,
            at: OffsetDateTime::now_utc(),
            from: from_kind,
            to: to_kind,
            cause,
            worker,
            note,
        });
    }
}

/// Duration and worker metadata captured for each pipeline stage.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StageSpans {
    #[serde(default)]
    spans: BTreeMap<PipelineStage, StageSpan>,
}

impl StageSpans {
    pub fn start(&mut self, stage: PipelineStage, worker_id: Option<String>) {
        self.spans.insert(
            stage,
            StageSpan {
                started_at: OffsetDateTime::now_utc(),
                finished_at: None,
                worker_id,
            },
        );
    }

    pub fn finish(&mut self, stage: PipelineStage) {
        if let Some(span) = self.spans.get_mut(&stage) {
            span.finished_at = Some(OffsetDateTime::now_utc());
        }
    }

    pub fn get(&self, stage: &PipelineStage) -> Option<&StageSpan> {
        self.spans.get(stage)
    }

    pub fn set_worker(&mut self, stage: PipelineStage, worker_id: Option<String>) {
        if let Some(span) = self.spans.get_mut(&stage) {
            span.worker_id = worker_id;
        }
    }
}

/// Snapshot of the timing information for a single stage.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StageSpan {
    pub started_at: OffsetDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<OffsetDateTime>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
}

/// Enumerates the discrete processing stages in the pipeline.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStage {
    Ingestion,
    Filtering,
    Transformation,
    Synthesis,
    Playback,
}

/// Record of a single lifecycle transition.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransitionLog {
    pub seq: u64,
    pub at: OffsetDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<MessageStateKind>,
    pub to: MessageStateKind,
    pub cause: TransitionCause,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Encodes why a transition occurred.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionCause {
    Automatic,
    ManualOverride,
    System,
    Retry,
}

/// High-level lifecycle state categories.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageStateKind {
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

/// Detailed lifecycle state information.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum MessageState {
    Arrived,
    Filtered,
    Transforming(TransformingState),
    Transformed(TransformedState),
    Synthesizing(SynthesizingState),
    Synthesized(SynthesizedState),
    Queued(QueuedState),
    Playing(PlayingState),
    Played(PlayedState),
    Skipped(SkippedState),
    Error(ErrorState),
}

impl MessageState {
    pub fn kind(&self) -> MessageStateKind {
        match self {
            MessageState::Arrived => MessageStateKind::Arrived,
            MessageState::Filtered => MessageStateKind::Filtered,
            MessageState::Transforming(_) => MessageStateKind::Transforming,
            MessageState::Transformed(_) => MessageStateKind::Transformed,
            MessageState::Synthesizing(_) => MessageStateKind::Synthesizing,
            MessageState::Synthesized(_) => MessageStateKind::Synthesized,
            MessageState::Queued(_) => MessageStateKind::Queued,
            MessageState::Playing(_) => MessageStateKind::Playing,
            MessageState::Played(_) => MessageStateKind::Played,
            MessageState::Skipped(_) => MessageStateKind::Skipped,
            MessageState::Error(_) => MessageStateKind::Error,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransformingState {
    pub started_at: OffsetDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransformedState {
    pub completed_at: OffsetDateTime,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SynthesizingState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<SynthBackendId>,
    pub attempt: u32,
    pub started_at: OffsetDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SynthesizedState {
    pub artifact_key: String,
    pub backend: SynthBackendId,
    pub duration_ms: u32,
    pub completed_at: OffsetDateTime,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueuedState {
    pub queued_at: OffsetDateTime,
    pub priority: QueuePriority,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayingState {
    pub started_at: OffsetDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayedState {
    pub completed_at: OffsetDateTime,
    pub duration_ms: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkippedState {
    pub at: OffsetDateTime,
    pub reason: SkipReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorState {
    pub at: OffsetDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<PipelineStage>,
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    OperatorOverride,
    FilterRule,
    CommandFiltered,
    CommandRejected,
    ProviderBotSuppressed,
    PaidMessageSuppressed,
    PredictedQueueOverflow,
    QueueOverflow,
    ProviderRejected,
    ModeratorDeleted,
    Other(String),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueuePriority {
    #[default]
    Normal,
    High,
    Urgent,
}

/// Identifies the synthesizer backend that produced an artifact.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SynthBackendId(pub String);

impl SynthBackendId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for SynthBackendId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for SynthBackendId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Artifact metadata keyed by logical identifier.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ArtifactSet {
    #[serde(default)]
    entries: BTreeMap<String, Artifact>,
}

impl ArtifactSet {
    pub fn insert(&mut self, key: impl Into<String>, artifact: Artifact) {
        self.entries.insert(key.into(), artifact);
    }

    pub fn get(&self, key: &str) -> Option<&Artifact> {
        self.entries.get(key)
    }

    pub fn remove(&mut self, key: &str) -> Option<Artifact> {
        self.entries.remove(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Artifact)> {
        self.entries.iter()
    }
}

/// Supported artifact variants.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Artifact {
    Audio(AudioArtifact),
}

/// Metadata for stored audio content.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioArtifact {
    pub format: AudioFormat,
    pub storage_key: String,
    pub duration_ms: u32,
    pub sha256: String,
    pub backend: SynthBackendId,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum AudioFormat {
    #[default]
    Wav,
    Mp3,
    Ogg,
    Other(String),
}

#[derive(Clone, Debug)]
pub struct NewMessage {
    pub provider: String,
    pub provider_message_id: Option<String>,
    pub channel_id: Option<String>,
    pub user_id: Option<String>,
    pub sender: String,
    pub content: String,
    pub source_timestamp: Option<OffsetDateTime>,
    pub received_at: OffsetDateTime,
    pub delivery_kind: DeliveryKind,
    pub payment: Option<PaymentInfo>,
    pub metadata: BTreeMap<String, JsonValue>,
}

impl NewMessage {
    pub fn new<P, S, C>(provider: P, sender: S, content: C, received_at: OffsetDateTime) -> Self
    where
        P: Into<String>,
        S: Into<String>,
        C: Into<String>,
    {
        Self {
            provider: provider.into(),
            provider_message_id: None,
            channel_id: None,
            user_id: None,
            sender: sender.into(),
            content: content.into(),
            source_timestamp: None,
            received_at,
            delivery_kind: DeliveryKind::Standard,
            payment: None,
            metadata: BTreeMap::new(),
        }
    }

    pub fn manual<S, C>(sender: S, content: C) -> Self
    where
        S: Into<String>,
        C: Into<String>,
    {
        Self::new("manual", sender, content, OffsetDateTime::now_utc())
    }

    pub fn with_provider_message_id(mut self, value: impl Into<String>) -> Self {
        self.provider_message_id = Some(value.into());
        self
    }

    pub fn with_channel_id(mut self, value: impl Into<String>) -> Self {
        self.channel_id = Some(value.into());
        self
    }

    pub fn with_user_id(mut self, value: impl Into<String>) -> Self {
        self.user_id = Some(value.into());
        self
    }

    pub fn with_source_timestamp(mut self, value: OffsetDateTime) -> Self {
        self.source_timestamp = Some(value);
        self
    }

    pub fn with_delivery_kind(mut self, value: DeliveryKind) -> Self {
        self.delivery_kind = value;
        self
    }

    pub fn with_payment(mut self, value: PaymentInfo) -> Self {
        self.payment = Some(value);
        self
    }

    pub fn with_metadata(mut self, key: impl Into<String>, value: JsonValue) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }
}

/// Retrieve every persisted message ordered by newest-first.
pub fn list_messages_desc(db: &DatabaseHandle) -> Result<Vec<MessageSnapshot>, Error> {
    let txn = db.begin_read()?;
    let table = txn.open_table(MESSAGES_TABLE)?;
    let mut snapshots = Vec::new();
    for entry in table.iter()? {
        let (id_guard, value_guard) = entry?;
        let id = id_guard.value();
        let bytes = value_guard.value();
        let snapshot = decode_snapshot(id, bytes)?;
        snapshots.push(snapshot);
    }
    snapshots.sort_by(|a, b| b.header.id.cmp(&a.header.id));
    Ok(snapshots)
}

/// Retrieve a single message snapshot by identifier.
pub fn load_message(db: &DatabaseHandle, id: u64) -> Result<Option<MessageSnapshot>, Error> {
    let txn = db.begin_read()?;
    let table = txn.open_table(MESSAGES_TABLE)?;
    if let Some(value_guard) = table.get(id)? {
        let bytes = value_guard.value();
        let snapshot = decode_snapshot(id, bytes)?;
        Ok(Some(snapshot))
    } else {
        Ok(None)
    }
}

/// Persist an updated message snapshot.
pub fn save_message_snapshot(db: &DatabaseHandle, snapshot: &MessageSnapshot) -> Result<(), Error> {
    with_write_txn(db, |txn| {
        let mut table = txn.open_table(MESSAGES_TABLE)?;
        let bytes = encode_snapshot(snapshot)?;
        table.insert(snapshot.header.id, bytes.as_slice())?;
        Ok(())
    })
}

/// Create a new message entry and return the hydrated snapshot.
pub fn enqueue_message(db: &DatabaseHandle, request: NewMessage) -> Result<MessageSnapshot, Error> {
    with_write_txn(db, |txn| {
        let next_id = {
            let mut meta = txn.open_table(METADATA_TABLE)?;
            let next = meta
                .get(MESSAGES_NEXT_ID_KEY)?
                .map(|guard| guard.value())
                .unwrap_or(1);
            meta.insert(MESSAGES_NEXT_ID_KEY, next + 1)?;
            next
        };

        let mut table = txn.open_table(MESSAGES_TABLE)?;

        let header = MessageHeader {
            id: next_id,
            provider: request.provider,
            provider_message_id: request.provider_message_id,
            channel_id: request.channel_id,
            user_id: request.user_id,
            sender: request.sender,
            content: request.content,
            source_timestamp: request.source_timestamp,
            received_at: request.received_at,
            delivery_kind: request.delivery_kind,
            payment: request.payment,
            metadata: request.metadata,
        };
        let lifecycle = Lifecycle::new(MessageState::Arrived);
        let snapshot = MessageSnapshot {
            header,
            lifecycle,
            artifacts: ArtifactSet::default(),
        };
        let bytes = encode_snapshot(&snapshot)?;
        table.insert(next_id, bytes.as_slice())?;

        Ok(snapshot)
    })
}

/// Store (or overwrite) an audio artifact for a message.
pub fn store_audio_artifact(
    db: &DatabaseHandle,
    message_id: u64,
    storage_key: &str,
    bytes: &[u8],
) -> Result<(), Error> {
    let key = artifact_storage_key(message_id, storage_key);
    with_write_txn(db, |txn| {
        let mut table = txn.open_table(MESSAGE_ARTIFACTS_TABLE)?;
        table.insert(key.as_str(), bytes)?;
        Ok(())
    })
}

/// Load an artifact from storage if it exists.
pub fn load_audio_artifact(
    db: &DatabaseHandle,
    message_id: u64,
    storage_key: &str,
) -> Result<Option<Vec<u8>>, Error> {
    let key = artifact_storage_key(message_id, storage_key);
    let txn = db.begin_read()?;
    let table = txn.open_table(MESSAGE_ARTIFACTS_TABLE)?;
    let bytes = table.get(key.as_str())?.map(|guard| guard.value().to_vec());
    Ok(bytes)
}

/// Remove an artifact entry from storage.
pub fn delete_audio_artifact(
    db: &DatabaseHandle,
    message_id: u64,
    storage_key: &str,
) -> Result<(), Error> {
    let key = artifact_storage_key(message_id, storage_key);
    with_write_txn(db, |txn| {
        let mut table = txn.open_table(MESSAGE_ARTIFACTS_TABLE)?;
        table.remove(key.as_str())?;
        Ok(())
    })
}

/// Remove every persisted message and associated artifacts.
pub fn clear_all_messages(db: &DatabaseHandle) -> Result<(), Error> {
    with_write_txn(db, |txn| {
        let mut messages = txn.open_table(MESSAGES_TABLE)?;
        let mut artifacts = txn.open_table(MESSAGE_ARTIFACTS_TABLE)?;

        let mut artifact_keys = Vec::new();
        let mut message_ids = Vec::new();

        for entry in messages.iter()? {
            let (id_guard, value_guard) = entry?;
            let id = id_guard.value();
            let snapshot = decode_snapshot(id, value_guard.value())?;
            for (_, artifact) in snapshot.artifacts.iter() {
                match artifact {
                    Artifact::Audio(audio) => {
                        artifact_keys.push(artifact_storage_key(id, &audio.storage_key));
                    }
                }
            }
            message_ids.push(id);
        }

        for key in artifact_keys {
            artifacts.remove(key.as_str())?;
        }

        for id in message_ids {
            messages.remove(id)?;
        }

        Ok(())
    })
}

#[derive(Serialize, Deserialize)]
struct PersistedMessageV2 {
    version: u16,
    header: MessageHeader,
    lifecycle: Lifecycle,
    artifacts: ArtifactSet,
}

fn encode_snapshot(snapshot: &MessageSnapshot) -> Result<Vec<u8>, Error> {
    let payload = PersistedMessageV2 {
        version: CURRENT_MESSAGE_VERSION,
        header: snapshot.header.clone(),
        lifecycle: snapshot.lifecycle.clone(),
        artifacts: snapshot.artifacts.clone(),
    };
    Ok(serde_json::to_vec(&payload)?)
}

fn decode_snapshot(id: u64, bytes: &[u8]) -> Result<MessageSnapshot, Error> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let Some(version) = value.get("version").and_then(|v| v.as_u64()) else {
        return Err(Error::Serialization(SerdeDeError::custom(
            "missing message payload version; reset local database for phase-1 schema",
        )));
    };

    if version != CURRENT_MESSAGE_VERSION as u64 {
        return Err(Error::Serialization(SerdeDeError::custom(format!(
            "unsupported message payload version {version}; expected {CURRENT_MESSAGE_VERSION}",
        ))));
    }

    let mut payload: PersistedMessageV2 = serde_json::from_value(value)?;
    // Trust the key as the canonical identifier.
    payload.header.id = id;
    Ok(MessageSnapshot {
        header: payload.header,
        lifecycle: payload.lifecycle,
        artifacts: payload.artifacts,
    })
}

fn artifact_storage_key(message_id: u64, storage_key: &str) -> String {
    format!("{}:{}", message_id, storage_key)
}

/// Helper used by consumers that only need read access during a single transaction.
pub fn with_read_txn<F, T>(db: &DatabaseHandle, f: F) -> Result<T, Error>
where
    F: FnOnce(ReadTransaction) -> Result<T, Error>,
{
    let txn = db.begin_read()?;
    f(txn)
}

/// Helper used by consumers that need write access during a single transaction.
pub fn with_write_txn<F, T>(db: &DatabaseHandle, f: F) -> Result<T, Error>
where
    F: FnOnce(&mut WriteTransaction) -> Result<T, Error>,
{
    let mut txn = db.begin_write()?;
    let result = f(&mut txn)?;
    txn.commit()?;
    Ok(result)
}

/// Convenience helper to store arbitrary JSON blobs in a table.
pub fn store_json(
    txn: &mut WriteTransaction,
    table: TableDefinition<&str, &[u8]>,
    key: &str,
    value: &JsonValue,
) -> Result<(), Error> {
    let mut table = txn.open_table(table)?;
    let bytes = serde_json::to_vec(value)?;
    table.insert(key, bytes.as_slice())?;
    Ok(())
}

/// Convenience helper to remove a JSON entry.
pub fn delete_json(
    txn: &mut WriteTransaction,
    table: TableDefinition<&str, &[u8]>,
    key: &str,
) -> Result<(), Error> {
    let mut table = txn.open_table(table)?;
    table.remove(key)?;
    Ok(())
}

/// Load a JSON runtime blob from the application state table.
pub fn load_app_state_json(db: &DatabaseHandle, key: &str) -> Result<Option<JsonValue>, Error> {
    with_read_txn(db, |txn| {
        let table = txn.open_table(APP_STATE_TABLE)?;
        if let Some(value_guard) = table.get(key)? {
            let value = serde_json::from_slice::<JsonValue>(value_guard.value())?;
            Ok(Some(value))
        } else {
            Ok(None)
        }
    })
}

/// Store a JSON runtime blob in the application state table.
pub fn save_app_state_json(db: &DatabaseHandle, key: &str, value: &JsonValue) -> Result<(), Error> {
    with_write_txn(db, |txn| {
        store_json(txn, APP_STATE_TABLE, key, value)?;
        Ok(())
    })
}

/// Remove a JSON runtime blob from the application state table.
pub fn delete_app_state_json(db: &DatabaseHandle, key: &str) -> Result<(), Error> {
    with_write_txn(db, |txn| {
        delete_json(txn, APP_STATE_TABLE, key)?;
        Ok(())
    })
}

/// List all persisted chatter overrides ordered by chatter identifier.
pub fn list_chatter_overrides(db: &DatabaseHandle) -> Result<Vec<ChatterOverrideRecord>, Error> {
    with_read_txn(db, |txn| {
        let table = txn.open_table(CHATTER_OVERRIDES_TABLE)?;
        let mut rows = Vec::new();
        for entry in table.iter()? {
            let (_key_guard, value_guard) = entry?;
            let record = serde_json::from_slice::<ChatterOverrideRecord>(value_guard.value())?;
            rows.push(record);
        }
        rows.sort_by(|a, b| a.chatter_id.cmp(&b.chatter_id));
        Ok(rows)
    })
}

/// Load a single chatter override by canonical chatter identifier.
pub fn load_chatter_override(
    db: &DatabaseHandle,
    chatter_id: &str,
) -> Result<Option<ChatterOverrideRecord>, Error> {
    with_read_txn(db, |txn| {
        let table = txn.open_table(CHATTER_OVERRIDES_TABLE)?;
        if let Some(value_guard) = table.get(chatter_id)? {
            let record = serde_json::from_slice::<ChatterOverrideRecord>(value_guard.value())?;
            Ok(Some(record))
        } else {
            Ok(None)
        }
    })
}

/// Insert or replace a chatter override.
pub fn save_chatter_override(
    db: &DatabaseHandle,
    record: &ChatterOverrideRecord,
) -> Result<(), Error> {
    with_write_txn(db, |txn| {
        let mut table = txn.open_table(CHATTER_OVERRIDES_TABLE)?;
        let bytes = serde_json::to_vec(record)?;
        table.insert(record.chatter_id.as_str(), bytes.as_slice())?;
        Ok(())
    })
}

/// Delete a chatter override by canonical chatter identifier.
pub fn delete_chatter_override(db: &DatabaseHandle, chatter_id: &str) -> Result<(), Error> {
    with_write_txn(db, |txn| {
        let mut table = txn.open_table(CHATTER_OVERRIDES_TABLE)?;
        table.remove(chatter_id)?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn open_test_db() -> (TempDir, DatabaseHandle) {
        let temp = tempfile::tempdir().expect("failed to create temp dir");
        let runtime = tokio::runtime::Runtime::new().expect("failed to create runtime");
        let db = runtime
            .block_on(async { open_database_at_path(temp.path()).await })
            .expect("failed to open test db");
        (temp, db)
    }

    #[test]
    fn snapshot_round_trip_preserves_canonical_header_fields() {
        let mut metadata = BTreeMap::new();
        metadata.insert("is_moderator".to_string(), json!(true));
        metadata.insert("source".to_string(), json!("eventsub"));

        let snapshot = MessageSnapshot {
            header: MessageHeader {
                id: 42,
                provider: "twitch_chat".to_string(),
                provider_message_id: Some("abc123".to_string()),
                channel_id: Some("98765".to_string()),
                user_id: Some("12345".to_string()),
                sender: "alice".to_string(),
                content: "!lupo skip 42".to_string(),
                source_timestamp: Some(OffsetDateTime::now_utc()),
                received_at: OffsetDateTime::now_utc(),
                delivery_kind: DeliveryKind::Paid,
                payment: Some(PaymentInfo {
                    amount_minor_units: 500,
                    currency_code: "bits".to_string(),
                    provider_tier_code: Some("500".to_string()),
                    provider_color_rank: Some("purple".to_string()),
                }),
                metadata,
            },
            lifecycle: Lifecycle::new(MessageState::Arrived),
            artifacts: ArtifactSet::default(),
        };

        let encoded = encode_snapshot(&snapshot).expect("encode should succeed");
        let decoded = decode_snapshot(snapshot.header.id, &encoded).expect("decode should succeed");

        assert_eq!(decoded.header.provider, "twitch_chat");
        assert_eq!(
            decoded.header.provider_message_id.as_deref(),
            Some("abc123")
        );
        assert_eq!(decoded.header.channel_id.as_deref(), Some("98765"));
        assert_eq!(decoded.header.user_id.as_deref(), Some("12345"));
        assert_eq!(decoded.header.delivery_kind, DeliveryKind::Paid);
        assert_eq!(
            decoded
                .header
                .payment
                .as_ref()
                .map(|payment| payment.amount_minor_units),
            Some(500)
        );
        assert_eq!(
            decoded
                .header
                .metadata
                .get("is_moderator")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn canonical_chatter_id_prefers_user_id_then_sender() {
        let by_user = canonical_chatter_id("Twitch_Chat", Some(" 12345 "), "Alice");
        assert_eq!(by_user, "twitch_chat:user:12345");

        let by_sender = canonical_chatter_id("local", None, "  Alice  Example ");
        assert_eq!(by_sender, "local:sender:alice example");
    }

    #[test]
    fn chatter_override_crud_round_trip() {
        let (_temp, db) = open_test_db();
        let chatter_id = canonical_chatter_id("twitch_chat", Some("123"), "alice");
        let record = ChatterOverrideRecord {
            chatter_id: chatter_id.clone(),
            provider: "twitch_chat".to_string(),
            user_id: Some("123".to_string()),
            voice_override: Some("nova".to_string()),
            display_name_override: Some("Alice Prime".to_string()),
            notes: Some("VIP".to_string()),
            updated_at: OffsetDateTime::now_utc(),
        };

        save_chatter_override(&db, &record).expect("save should succeed");
        let loaded = load_chatter_override(&db, &chatter_id)
            .expect("load should succeed")
            .expect("record should exist");
        assert_eq!(loaded.voice_override.as_deref(), Some("nova"));

        let listed = list_chatter_overrides(&db).expect("list should succeed");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].chatter_id, chatter_id);

        delete_chatter_override(&db, &record.chatter_id).expect("delete should succeed");
        let missing = load_chatter_override(&db, &record.chatter_id).expect("load should work");
        assert!(missing.is_none());
    }

    #[test]
    fn chatter_override_overwrite_replaces_existing_value() {
        let (_temp, db) = open_test_db();
        let chatter_id = canonical_chatter_id("youtube_live_chat", Some("UC1"), "streamer");

        save_chatter_override(
            &db,
            &ChatterOverrideRecord {
                chatter_id: chatter_id.clone(),
                provider: "youtube_live_chat".to_string(),
                user_id: Some("UC1".to_string()),
                voice_override: Some("alloy".to_string()),
                display_name_override: None,
                notes: None,
                updated_at: OffsetDateTime::now_utc(),
            },
        )
        .expect("first save should succeed");

        save_chatter_override(
            &db,
            &ChatterOverrideRecord {
                chatter_id: chatter_id.clone(),
                provider: "youtube_live_chat".to_string(),
                user_id: Some("UC1".to_string()),
                voice_override: Some("echo".to_string()),
                display_name_override: Some("Stream Host".to_string()),
                notes: Some("override updated".to_string()),
                updated_at: OffsetDateTime::now_utc(),
            },
        )
        .expect("second save should succeed");

        let loaded = load_chatter_override(&db, &chatter_id)
            .expect("load should succeed")
            .expect("record should exist");
        assert_eq!(loaded.voice_override.as_deref(), Some("echo"));
        assert_eq!(loaded.display_name_override.as_deref(), Some("Stream Host"));
    }
}

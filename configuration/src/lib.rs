// SPDX-License-Identifier: Apache-2.0

//! Settings schema definitions and persistence helpers shared across
//! the LUPO workspace.

use lupo_persistence::{
    DatabaseHandle, Error as PersistenceError, SETTINGS_TABLE, with_read_txn, with_write_txn,
};
use once_cell::sync::Lazy;
use redb::{ReadableTable, ReadableTableMetadata};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use thiserror::Error;
use time::OffsetDateTime;
use tracing::warn;

/// Functional grouping used by the Settings and Advanced Settings views.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingDomain {
    Connections,
    MessageFiltering,
    CommandAcceptance,
    TextTransformation,
    Tts,
    OfflineTts,
    VoiceStrategy,
    AudioOutput,
    Advanced,
}

impl SettingDomain {
    pub fn as_str(self) -> &'static str {
        match self {
            SettingDomain::Connections => "connections",
            SettingDomain::MessageFiltering => "message_filtering",
            SettingDomain::CommandAcceptance => "command_acceptance",
            SettingDomain::TextTransformation => "text_transformation",
            SettingDomain::Tts => "tts",
            SettingDomain::OfflineTts => "offline_tts",
            SettingDomain::VoiceStrategy => "voice_strategy",
            SettingDomain::AudioOutput => "audio_output",
            SettingDomain::Advanced => "advanced",
        }
    }
}

impl fmt::Display for SettingDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SettingDomain {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "connections" => Ok(SettingDomain::Connections),
            "message_filtering" => Ok(SettingDomain::MessageFiltering),
            "command_acceptance" => Ok(SettingDomain::CommandAcceptance),
            "text_transformation" => Ok(SettingDomain::TextTransformation),
            "tts" => Ok(SettingDomain::Tts),
            "offline_tts" => Ok(SettingDomain::OfflineTts),
            "voice_strategy" => Ok(SettingDomain::VoiceStrategy),
            "audio_output" => Ok(SettingDomain::AudioOutput),
            "advanced" => Ok(SettingDomain::Advanced),
            _ => Err(()),
        }
    }
}

/// Risk profile surfaced to the Advanced Settings UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl RiskLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            RiskLevel::Low => "low",
            RiskLevel::Medium => "medium",
            RiskLevel::High => "high",
        }
    }
}

/// Coarse value categorisation used to validate CLI input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingValueKind {
    Boolean,
    Integer,
    Number,
    String,
    Object,
    Array,
    Enum { variants: &'static [&'static str] },
}

/// Metadata that describes a single configuration key.
pub struct SettingSchema {
    pub key: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub domain: SettingDomain,
    pub risk: RiskLevel,
    pub value_kind: SettingValueKind,
    pub default_value: fn() -> Value,
}

/// Returns the canonical list of configuration keys.
pub fn all_schemas() -> &'static [SettingSchema] {
    &SETTINGS_REGISTRY
}

/// Lookup table from key → schema for quick validation.
static SCHEMA_INDEX: Lazy<HashMap<&'static str, &'static SettingSchema>> = Lazy::new(|| {
    let mut map = HashMap::new();
    for schema in SETTINGS_REGISTRY.iter() {
        map.insert(schema.key, schema);
    }
    map
});

/// The ordered registry surfaced through CLI listings and exports.
static SETTINGS_REGISTRY: [SettingSchema; 15] = [
    SettingSchema {
        key: "connections",
        title: "Connections",
        description: "Chat and TTS provider connections, authentication state, and reconnection policies.",
        domain: SettingDomain::Connections,
        risk: RiskLevel::Medium,
        value_kind: SettingValueKind::Object,
        default_value: || {
            json!({
                "providers": [],
                "auto_reconnect": true,
                "status_poll_interval_secs": 60,
                "twitch_channel_login": null,
                "youtube_live_chat_id": null
            })
        },
    },
    SettingSchema {
        key: "message_filtering",
        title: "Message filtering",
        description: "Deterministic filters that decide whether a message enters the synthesis pipeline.",
        domain: SettingDomain::MessageFiltering,
        risk: RiskLevel::Medium,
        value_kind: SettingValueKind::Object,
        default_value: || {
            json!({
                "mode": "blacklist",
                "rules": {
                    "ignore_prefix": "!",
                    "command_prefix": "!lupo",
                    "suppress_provider_bots": true,
                    "ignore_paid": false
                }
            })
        },
    },
    SettingSchema {
        key: "command_acceptance",
        title: "Command acceptance",
        description: "Defines which chatter roles may issue chat-based control commands.",
        domain: SettingDomain::CommandAcceptance,
        risk: RiskLevel::Low,
        value_kind: SettingValueKind::Enum {
            variants: &["nobody", "moderators", "channel_owner"],
        },
        default_value: || Value::String("moderators".into()),
    },
    SettingSchema {
        key: "text_transformation",
        title: "Text transformation",
        description: "Template system that adjusts how synthesized messages are phrased.",
        domain: SettingDomain::TextTransformation,
        risk: RiskLevel::Low,
        value_kind: SettingValueKind::Object,
        default_value: || {
            json!({
                "template": "{user} said {message}",
                "strip_emotes": true,
                "normalize_whitespace": true
            })
        },
    },
    SettingSchema {
        key: "tts",
        title: "TTS provider selection",
        description: "Preferred synthesis providers, fallback ordering, and provider-specific configuration.",
        domain: SettingDomain::Tts,
        risk: RiskLevel::Medium,
        value_kind: SettingValueKind::Object,
        default_value: || {
            json!({
                "providers": [
                    {
                        "id": "system_tts",
                        "enabled": true
                    },
                    {
                        "id": "openai",
                        "enabled": true,
                        "voice": "alloy",
                        "stability": 0.15
                    }
                ],
                "fallback_strategy": "ordered",
                "retry_limit": 3
            })
        },
    },
    SettingSchema {
        key: "offline_tts_strategy",
        title: "Offline TTS strategy",
        description: "Local engines, optional downloads, and activation preferences for offline synthesis.",
        domain: SettingDomain::OfflineTts,
        risk: RiskLevel::Low,
        value_kind: SettingValueKind::Object,
        default_value: || {
            json!({
                "engine": "parler",
                "language_packs": ["default"],
                "enable_ultra_light": false
            })
        },
    },
    SettingSchema {
        key: "voice_strategy",
        title: "Voice strategy",
        description: "Rules for per-user overrides, broadcast defaults, and message-type prioritisation.",
        domain: SettingDomain::VoiceStrategy,
        risk: RiskLevel::Medium,
        value_kind: SettingValueKind::Object,
        default_value: || {
            json!({
                "mode": "broadcast_default",
                "default_voice": "operator",
                "per_user": [],
                "message_type_overrides": {}
            })
        },
    },
    SettingSchema {
        key: "audio_output",
        title: "Audio output",
        description: "Playback routing, gain, and device selection.",
        domain: SettingDomain::AudioOutput,
        risk: RiskLevel::Low,
        value_kind: SettingValueKind::Object,
        default_value: || {
            json!({
                "device": "system_default",
                "fallback_device": null,
                "gain_db": 0.0
            })
        },
    },
    SettingSchema {
        key: "advanced",
        title: "Advanced settings",
        description: "Expert-only configuration for queue sizing, concurrency caps, and experimental flags.",
        domain: SettingDomain::Advanced,
        risk: RiskLevel::High,
        value_kind: SettingValueKind::Object,
        default_value: || {
            json!({
                "pipeline": {
                    "arrived_queue": 128,
                    "filtered_queue": 128,
                    "synthesis_queue": 32
                },
                "scheduler": {
                    "max_parallel_synthesis": 4,
                    "max_predicted_queue_duration_secs": 600
                },
                "feature_flags": {
                    "enable_latency_probe": false
                }
            })
        },
    },
    SettingSchema {
        key: "encryption_key",
        title: "Encryption key metadata",
        description: "Legacy metadata retained from the deprecated Keychain-backed encryption flow.",
        domain: SettingDomain::Advanced,
        risk: RiskLevel::High,
        value_kind: SettingValueKind::Object,
        default_value: || json!({}),
    },
    SettingSchema {
        key: "secret_openai_api_key",
        title: "OpenAI API key (secret store)",
        description: "Metadata describing the locally stored OpenAI API key.",
        domain: SettingDomain::Connections,
        risk: RiskLevel::High,
        value_kind: SettingValueKind::Object,
        default_value: || json!({}),
    },
    SettingSchema {
        key: "secret_twitch_access_token",
        title: "Twitch access token (secret store)",
        description: "Metadata describing the locally stored Twitch access token.",
        domain: SettingDomain::Connections,
        risk: RiskLevel::High,
        value_kind: SettingValueKind::Object,
        default_value: || json!({}),
    },
    SettingSchema {
        key: "secret_twitch_refresh_token",
        title: "Twitch refresh token (secret store)",
        description: "Metadata describing the locally stored Twitch refresh token.",
        domain: SettingDomain::Connections,
        risk: RiskLevel::High,
        value_kind: SettingValueKind::Object,
        default_value: || json!({}),
    },
    SettingSchema {
        key: "secret_youtube_api_key",
        title: "YouTube API key (secret store)",
        description: "Metadata describing the locally stored YouTube Data API key.",
        domain: SettingDomain::Connections,
        risk: RiskLevel::High,
        value_kind: SettingValueKind::Object,
        default_value: || json!({}),
    },
    SettingSchema {
        key: "twitch_auth_state",
        title: "Twitch authorization metadata",
        description: "Metadata describing the Twitch account currently linked to LUPO.",
        domain: SettingDomain::Connections,
        risk: RiskLevel::Medium,
        value_kind: SettingValueKind::Object,
        default_value: || json!({}),
    },
];

fn schema_for_key(key: &str) -> Option<&'static SettingSchema> {
    SCHEMA_INDEX.get(key).copied()
}

fn validate_value(schema: &SettingSchema, value: &Value) -> Result<(), Error> {
    match schema.value_kind {
        SettingValueKind::Boolean => {
            if !value.is_boolean() {
                return Err(Error::Validation {
                    key: schema.key.to_string(),
                    message: format!("expected boolean value, got {}", value_describe(value)),
                });
            }
        }
        SettingValueKind::Integer => {
            if !value.is_i64() && !value.is_u64() {
                return Err(Error::Validation {
                    key: schema.key.to_string(),
                    message: format!("expected integer value, got {}", value_describe(value)),
                });
            }
        }
        SettingValueKind::Number => {
            if !value.is_number() {
                return Err(Error::Validation {
                    key: schema.key.to_string(),
                    message: format!("expected numeric value, got {}", value_describe(value)),
                });
            }
        }
        SettingValueKind::String => {
            if !value.is_string() {
                return Err(Error::Validation {
                    key: schema.key.to_string(),
                    message: format!("expected string value, got {}", value_describe(value)),
                });
            }
        }
        SettingValueKind::Object => {
            if !value.is_object() {
                return Err(Error::Validation {
                    key: schema.key.to_string(),
                    message: format!("expected object value, got {}", value_describe(value)),
                });
            }
        }
        SettingValueKind::Array => {
            if !value.is_array() {
                return Err(Error::Validation {
                    key: schema.key.to_string(),
                    message: format!("expected array value, got {}", value_describe(value)),
                });
            }
        }
        SettingValueKind::Enum { variants } => {
            let Some(s) = value.as_str() else {
                return Err(Error::Validation {
                    key: schema.key.to_string(),
                    message: format!(
                        "expected string value for enum, got {}",
                        value_describe(value)
                    ),
                });
            };
            if !variants.iter().any(|candidate| candidate == &s) {
                return Err(Error::Validation {
                    key: schema.key.to_string(),
                    message: format!(
                        "value '{s}' is not one of the allowed variants: {}",
                        variants.join(", ")
                    ),
                });
            }
        }
    }
    Ok(())
}

fn value_describe(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Source that produced the effective value.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingSource {
    Default,
    Persisted {
        scope: String,
        updated_at: OffsetDateTime,
    },
}

/// Entry returned by the [`SettingsService`] list and get methods.
#[derive(Clone, Debug, Serialize)]
pub struct SettingRecord {
    pub key: String,
    pub title: String,
    pub description: String,
    pub domain: SettingDomain,
    pub risk: RiskLevel,
    pub value_kind: SettingValueKind,
    pub value: Value,
    pub source: SettingSource,
}

impl SettingRecord {
    fn from_schema(schema: &SettingSchema, stored: Option<StoredSetting>) -> Self {
        let (value, source) = match stored {
            Some(stored) => (
                stored.value,
                SettingSource::Persisted {
                    scope: stored.scope,
                    updated_at: stored.updated_at,
                },
            ),
            None => ((schema.default_value)(), SettingSource::Default),
        };

        Self {
            key: schema.key.to_string(),
            title: schema.title.to_string(),
            description: schema.description.to_string(),
            domain: schema.domain,
            risk: schema.risk,
            value_kind: schema.value_kind,
            value,
            source,
        }
    }
}

/// Stored settings that currently lack a schema entry.
#[derive(Clone, Debug, Serialize)]
pub struct OrphanRecord {
    pub key: String,
    pub value: Value,
    pub scope: String,
    pub updated_at: OffsetDateTime,
}

/// Listing result including known keys and orphaned rows.
#[derive(Clone, Debug, Serialize)]
pub struct SettingsListing {
    pub entries: Vec<SettingRecord>,
    pub orphans: Vec<OrphanRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredSetting {
    pub domain: String,
    pub scope: String,
    pub value: Value,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// Errors emitted by the service.
#[derive(Debug, Error)]
pub enum Error {
    #[error("unknown setting key: {0}")]
    UnknownSetting(String),
    #[error("validation failed for '{key}': {message}")]
    Validation { key: String, message: String },
    #[error("storage error")]
    Storage(#[from] PersistenceError),
}

/// Snapshot format used for exports/imports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsSnapshot {
    pub generated_at: OffsetDateTime,
    pub settings: BTreeMap<String, Value>,
}

impl SettingsSnapshot {
    pub fn new(settings: BTreeMap<String, Value>) -> Self {
        Self {
            generated_at: OffsetDateTime::now_utc(),
            settings,
        }
    }
}

/// Options used when applying a snapshot.
#[derive(Debug, Clone, Copy, Default)]
pub struct SnapshotApplyOptions {
    pub allow_unknown: bool,
    pub dry_run: bool,
}

/// Change report produced during snapshot application.
#[derive(Debug, Clone, Serialize)]
pub struct SettingChange {
    pub key: String,
    pub previous_value: Value,
    pub new_value: Value,
    pub source_before: SettingSource,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotReport {
    pub applied: Vec<SettingChange>,
    pub skipped_unknown: Vec<String>,
}

/// Primary façade that owns database access.
#[derive(Clone)]
pub struct SettingsService {
    db: DatabaseHandle,
}

impl SettingsService {
    pub fn new(db: DatabaseHandle) -> Self {
        Self { db }
    }

    pub fn database(&self) -> DatabaseHandle {
        self.db.clone()
    }

    fn load_all(&self) -> Result<HashMap<String, StoredSetting>, Error> {
        let map = with_read_txn(&self.db, |txn| {
            let table = txn.open_table(SETTINGS_TABLE)?;
            let mut map = HashMap::new();
            for entry in table.iter()? {
                let (key_guard, value_guard) = entry?;
                let key = key_guard.value().to_string();
                let stored: StoredSetting = serde_json::from_slice(value_guard.value())?;
                map.insert(key, stored);
            }
            Ok(map)
        })?;
        Ok(map)
    }

    fn load_one(&self, key: &str) -> Result<Option<StoredSetting>, Error> {
        let stored = with_read_txn(&self.db, |txn| {
            let table = txn.open_table(SETTINGS_TABLE)?;
            if let Some(value) = table.get(key)? {
                let stored: StoredSetting = serde_json::from_slice(value.value())?;
                Ok(Some(stored))
            } else {
                Ok(None)
            }
        })?;
        Ok(stored)
    }

    /// List every known configuration key and report orphaned rows.
    pub fn list(&self) -> Result<SettingsListing, Error> {
        let mut map = self.load_all()?;
        let mut entries = Vec::with_capacity(SETTINGS_REGISTRY.len());

        for schema in SETTINGS_REGISTRY.iter() {
            entries.push(SettingRecord::from_schema(schema, map.remove(schema.key)));
        }

        if !map.is_empty() {
            warn!(
                "found {} persisted settings without schema entries",
                map.len()
            );
        }

        let orphans = map
            .into_iter()
            .map(|(key, stored)| OrphanRecord {
                key,
                value: stored.value,
                scope: stored.scope,
                updated_at: stored.updated_at,
            })
            .collect();

        Ok(SettingsListing { entries, orphans })
    }

    /// Retrieve a single configuration entry (including defaults).
    pub fn get(&self, key: &str) -> Result<SettingRecord, Error> {
        let Some(schema) = schema_for_key(key) else {
            return Err(Error::UnknownSetting(key.to_string()));
        };

        Ok(SettingRecord::from_schema(schema, self.load_one(key)?))
    }

    /// Persist a value after schema validation.
    pub fn set(&self, key: &str, value: Value) -> Result<(), Error> {
        let Some(schema) = schema_for_key(key) else {
            return Err(Error::UnknownSetting(key.to_string()));
        };
        validate_value(schema, &value)?;

        let stored = StoredSetting {
            domain: schema.domain.as_str().to_string(),
            scope: "global".to_string(),
            value,
            updated_at: OffsetDateTime::now_utc(),
        };

        with_write_txn(&self.db, |txn| {
            let mut table = txn.open_table(SETTINGS_TABLE)?;
            let bytes = serde_json::to_vec(&stored)?;
            table.insert(key, bytes.as_slice())?;
            Ok(())
        })?;
        Ok(())
    }

    /// Remove any persisted value for the given key.
    pub fn reset(&self, key: &str) -> Result<(), Error> {
        let Some(_schema) = schema_for_key(key) else {
            return Err(Error::UnknownSetting(key.to_string()));
        };

        with_write_txn(&self.db, |txn| {
            let mut table = txn.open_table(SETTINGS_TABLE)?;
            table.remove(key)?;
            Ok(())
        })?;
        Ok(())
    }

    /// Remove every persisted override.
    pub fn reset_all(&self) -> Result<u64, Error> {
        let removed = with_write_txn(&self.db, |txn| {
            let mut table = txn.open_table(SETTINGS_TABLE)?;
            let len = table.len()?;
            let keys = table
                .iter()?
                .map(|entry| {
                    let (key_guard, _) = entry?;
                    Ok::<_, lupo_persistence::Error>(key_guard.value().to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            for key in keys {
                table.remove(key.as_str())?;
            }
            Ok(len)
        })?;
        Ok(removed)
    }

    /// Produce a serialisable snapshot of every effective value.
    pub fn snapshot(&self) -> Result<SettingsSnapshot, Error> {
        let listing = self.list()?;
        let mut map = BTreeMap::new();
        for entry in listing.entries {
            map.insert(entry.key, entry.value);
        }
        Ok(SettingsSnapshot::new(map))
    }

    /// Apply a previously exported snapshot.
    pub fn apply_snapshot(
        &self,
        snapshot: &SettingsSnapshot,
        options: SnapshotApplyOptions,
    ) -> Result<SnapshotReport, Error> {
        let listing = self.list()?;
        let mut current: HashMap<String, SettingRecord> = listing
            .entries
            .into_iter()
            .map(|entry| (entry.key.clone(), entry))
            .collect();

        let mut applied = Vec::new();
        let mut skipped_unknown = Vec::new();

        for (key, new_value) in snapshot.settings.iter() {
            let Some(schema) = schema_for_key(key) else {
                if options.allow_unknown {
                    skipped_unknown.push(key.clone());
                    continue;
                } else {
                    return Err(Error::UnknownSetting(key.clone()));
                }
            };

            validate_value(schema, new_value)?;
            let previous = current
                .remove(key)
                .expect("listing contains every schema entry");

            if previous.value != *new_value {
                if !options.dry_run {
                    self.set(key, new_value.clone())?;
                }
                applied.push(SettingChange {
                    key: key.clone(),
                    previous_value: previous.value,
                    new_value: new_value.clone(),
                    source_before: previous.source,
                });
            }
        }

        Ok(SnapshotReport {
            applied,
            skipped_unknown,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lupo_persistence::open_database_at_path;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use tokio::runtime::Runtime;

    fn test_service() -> (SettingsService, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let runtime = Runtime::new().expect("failed to create runtime");
        let db = runtime
            .block_on(async { open_database_at_path(temp_dir.path()).await })
            .expect("failed to open test database");
        (SettingsService::new(db), temp_dir)
    }

    #[test]
    fn list_and_get_agree_for_default_and_persisted_settings() {
        let (service, _temp_dir) = test_service();

        for persisted in [false, true] {
            if persisted {
                service
                    .set("command_acceptance", json!("channel_owner"))
                    .unwrap();
            }
            for listed in service.list().unwrap().entries {
                let fetched = service.get(&listed.key).unwrap();
                assert_eq!(
                    serde_json::to_value(&listed).unwrap(),
                    serde_json::to_value(&fetched).unwrap(),
                    "{}",
                    listed.key,
                );
            }
        }
    }

    #[test]
    fn set_rejects_enum_type_mismatch() {
        let (service, _temp_dir) = test_service();
        let error = service
            .set("command_acceptance", json!(42))
            .expect_err("setting should fail validation");

        match error {
            Error::Validation { key, message } => {
                assert_eq!(key, "command_acceptance");
                assert!(message.contains("expected string value for enum"));
            }
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn set_and_reset_round_trip_persistence() {
        let (service, _temp_dir) = test_service();

        service
            .set("command_acceptance", json!("channel_owner"))
            .expect("failed to persist setting");

        let persisted = service
            .get("command_acceptance")
            .expect("failed to load persisted setting");
        assert_eq!(persisted.value, json!("channel_owner"));
        assert!(matches!(persisted.source, SettingSource::Persisted { .. }));

        service
            .reset("command_acceptance")
            .expect("failed to reset setting");
        let reset = service
            .get("command_acceptance")
            .expect("failed to load reset setting");
        assert_eq!(reset.value, json!("moderators"));
        assert!(matches!(reset.source, SettingSource::Default));
    }

    #[test]
    fn apply_snapshot_dry_run_reports_changes_without_persisting() {
        let (service, _temp_dir) = test_service();
        let snapshot = SettingsSnapshot::new(BTreeMap::from([(
            "command_acceptance".to_string(),
            json!("nobody"),
        )]));

        let report = service
            .apply_snapshot(
                &snapshot,
                SnapshotApplyOptions {
                    allow_unknown: false,
                    dry_run: true,
                },
            )
            .expect("dry-run snapshot apply failed");

        assert_eq!(report.applied.len(), 1);
        assert!(report.skipped_unknown.is_empty());

        let record = service
            .get("command_acceptance")
            .expect("failed to read setting after dry-run");
        assert_eq!(record.value, json!("moderators"));
        assert!(matches!(record.source, SettingSource::Default));
    }

    #[test]
    fn apply_snapshot_persists_and_skips_unknown_keys_when_allowed() {
        let (service, _temp_dir) = test_service();
        let snapshot = SettingsSnapshot::new(BTreeMap::from([
            ("command_acceptance".to_string(), json!("channel_owner")),
            ("unknown_key".to_string(), json!(true)),
        ]));

        let report = service
            .apply_snapshot(
                &snapshot,
                SnapshotApplyOptions {
                    allow_unknown: true,
                    dry_run: false,
                },
            )
            .expect("snapshot apply failed");

        assert_eq!(report.applied.len(), 1);
        assert_eq!(report.applied[0].key, "command_acceptance");
        assert_eq!(report.skipped_unknown, vec!["unknown_key".to_string()]);

        let record = service
            .get("command_acceptance")
            .expect("failed to read persisted snapshot value");
        assert_eq!(record.value, json!("channel_owner"));
        assert!(matches!(record.source, SettingSource::Persisted { .. }));
    }

    #[test]
    fn default_tts_provider_order_is_system_then_openai() {
        let (service, _temp_dir) = test_service();
        let record = service.get("tts").expect("failed to load tts setting");
        let providers = record
            .value
            .get("providers")
            .and_then(Value::as_array)
            .expect("tts.providers should be an array");
        assert_eq!(providers.len(), 2);
        assert_eq!(
            providers[0].get("id").and_then(Value::as_str),
            Some("system_tts")
        );
        assert_eq!(
            providers[1].get("id").and_then(Value::as_str),
            Some("openai")
        );
    }
}

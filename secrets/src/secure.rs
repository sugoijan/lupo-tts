// SPDX-License-Identifier: Apache-2.0

use lupo_configuration::SettingsService;
use lupo_persistence::{SECRETS_TABLE, with_read_txn, with_write_txn};
use serde_json::json;
use std::string::FromUtf8Error;
use thiserror::Error;

/// Errors returned by the embedded secret store.
#[derive(Debug, Error)]
pub enum SecureStoreError {
    #[error("secret storage error")]
    Storage(#[from] lupo_persistence::Error),
    #[error("settings error")]
    Settings(#[from] lupo_configuration::Error),
    #[error("secret is not valid UTF-8")]
    InvalidUtf8(#[from] FromUtf8Error),
}

pub type SecureStoreResult<T> = Result<T, SecureStoreError>;

/// Persist a secret value for the supplied settings key.
pub fn store_secret(
    service: &SettingsService,
    setting_key: &str,
    plaintext: &[u8],
) -> SecureStoreResult<()> {
    with_write_txn(&service.database(), |txn| {
        let mut table = txn.open_table(SECRETS_TABLE)?;
        table.insert(setting_key, plaintext)?;
        Ok(())
    })?;

    let value = json!({
        "storage": "embedded_redb",
        "key": setting_key,
        "version": 1,
    });
    service.set(setting_key, value)?;
    Ok(())
}

/// Load the plaintext secret if present.
pub fn load_secret_string(
    service: &SettingsService,
    setting_key: &str,
) -> SecureStoreResult<Option<String>> {
    let maybe_bytes = with_read_txn(&service.database(), |txn| {
        let table = txn.open_table(SECRETS_TABLE)?;
        if let Some(value) = table.get(setting_key)? {
            Ok(Some(value.value().to_vec()))
        } else {
            Ok(None)
        }
    })?;

    let Some(bytes) = maybe_bytes else {
        return Ok(None);
    };

    let secret = String::from_utf8(bytes)?;
    Ok(Some(secret))
}

/// Remove a previously stored secret from the embedded store and metadata table.
pub fn delete_secret(service: &SettingsService, setting_key: &str) -> SecureStoreResult<()> {
    with_write_txn(&service.database(), |txn| {
        let mut table = txn.open_table(SECRETS_TABLE)?;
        table.remove(setting_key)?;
        Ok(())
    })?;

    service.reset(setting_key)?;
    Ok(())
}

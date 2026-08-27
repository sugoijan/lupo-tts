// SPDX-License-Identifier: Apache-2.0
//! LUPO-facing Twitch auth helpers backed by `barbed`.

use anyhow::Result;
use barbed::helix::HelixError;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin};
use time::{Duration as TimeDuration, OffsetDateTime};

pub use barbed::oauth::{TokenValidation, TwitchAuthConfig, TwitchDeviceAuthorization};

type StorageFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Convenience handle wrapping the native `barbed` auth client.
#[derive(Clone)]
pub struct TwitchAuthClient {
    inner: barbed::native::TwitchAuthClient,
}

impl TwitchAuthClient {
    pub fn new(config: TwitchAuthConfig) -> Result<Self> {
        Ok(Self {
            inner: barbed::native::TwitchAuthClient::new(config)?,
        })
    }

    pub fn config(&self) -> &TwitchAuthConfig {
        self.inner.config()
    }

    pub async fn start_device_code_flow(
        &self,
        scopes: &[String],
    ) -> Result<TwitchDeviceAuthorization> {
        self.inner.start_device_code_flow(scopes).await
    }

    pub async fn complete_device_code_flow(
        &self,
        authorization: &TwitchDeviceAuthorization,
    ) -> Result<TwitchAuthOutcome> {
        let outcome = self.inner.complete_device_code_flow(authorization).await?;
        Ok(TwitchAuthOutcome::from_barbed(outcome))
    }
}

/// Outcome of a completed Twitch login, including metadata and issued tokens.
#[derive(Debug, Clone)]
pub struct TwitchAuthOutcome {
    pub login_state: TwitchLoginState,
    pub tokens: TwitchTokenBundle,
}

impl TwitchAuthOutcome {
    fn from_barbed(value: barbed::oauth::TwitchAuthOutcome) -> Self {
        let login_state = TwitchLoginState {
            login: value.identity.login.clone(),
            display_name: Some(value.identity.display_name.clone()),
            user_id: value.identity.user_id.clone(),
            scopes: value.tokens.scope.clone(),
            obtained_at: OffsetDateTime::now_utc(),
        };
        Self {
            login_state,
            tokens: TwitchTokenBundle::from_barbed(value.tokens),
        }
    }
}

/// Serializable structure describing the Twitch identity we authenticated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwitchLoginState {
    pub login: String,
    pub display_name: Option<String>,
    pub user_id: String,
    pub scopes: Vec<String>,
    pub obtained_at: OffsetDateTime,
}

/// Token bundle returned by Twitch during OAuth exchanges.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwitchTokenBundle {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<u64>,
    pub scope: Vec<String>,
    pub token_type: String,
}

impl TwitchTokenBundle {
    fn from_barbed(value: barbed::oauth::TwitchTokenState) -> Self {
        Self {
            access_token: value.access_token,
            refresh_token: Some(value.refresh_token),
            expires_in: value.expires_in_seconds.map(u64::from),
            scope: value.scope,
            token_type: value.token_type,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTokenInfo {
    pub token_type: String,
    pub expires_in: Option<u64>,
    pub expires_at: Option<OffsetDateTime>,
    pub has_refresh_token: bool,
}

impl StoredTokenInfo {
    pub fn from_parts(state: &TwitchLoginState, tokens: &TwitchTokenBundle) -> Self {
        let expires_at = tokens
            .expires_in
            .and_then(|secs| secs.try_into().ok())
            .and_then(|secs: i64| state.obtained_at.checked_add(TimeDuration::seconds(secs)));

        Self {
            token_type: tokens.token_type.clone(),
            expires_in: tokens.expires_in,
            expires_at,
            has_refresh_token: tokens
                .refresh_token
                .as_ref()
                .map(|value| !value.is_empty())
                .unwrap_or(false),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAuthState {
    pub login_state: TwitchLoginState,
    pub token_info: StoredTokenInfo,
}

impl StoredAuthState {
    pub fn from_parts(state: &TwitchLoginState, tokens: &TwitchTokenBundle) -> Self {
        Self {
            login_state: state.clone(),
            token_info: StoredTokenInfo::from_parts(state, tokens),
        }
    }
}

/// Trait describing how LUPO persists and retrieves Twitch secrets.
pub trait TwitchTokenStorage {
    fn store_tokens<'a>(
        &'a self,
        state: &'a TwitchLoginState,
        tokens: &'a TwitchTokenBundle,
    ) -> StorageFuture<'a, Result<()>>;

    fn load_tokens(
        &self,
    ) -> StorageFuture<'_, Result<Option<(TwitchLoginState, TwitchTokenBundle)>>>;

    fn clear_tokens(&self) -> StorageFuture<'_, Result<()>>;
}

#[derive(Debug, Clone)]
pub struct TwitchUserIdentity {
    pub id: String,
    pub login: String,
    pub display_name: String,
}

/// Marker error raised when Helix returns HTTP 401.
#[derive(Debug)]
pub struct TwitchUnauthorizedError;

impl std::fmt::Display for TwitchUnauthorizedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Twitch API returned HTTP 401 Unauthorized")
    }
}

impl std::error::Error for TwitchUnauthorizedError {}

pub fn is_twitch_unauthorized(err: &anyhow::Error) -> bool {
    err.downcast_ref::<TwitchUnauthorizedError>().is_some()
}

pub async fn fetch_user_identity(
    http: &Client,
    client_id: &str,
    access_token: &str,
    login: &str,
) -> Result<TwitchUserIdentity> {
    match barbed::native::fetch_user_by_login(http, access_token, client_id, login).await {
        Ok(identity) => Ok(TwitchUserIdentity {
            id: identity.user_id,
            login: identity.login,
            display_name: identity.display_name,
        }),
        Err(err) => {
            if let Some(HelixError::ApiError { status: 401, .. }) = err.downcast_ref::<HelixError>()
            {
                Err(TwitchUnauthorizedError.into())
            } else {
                Err(err)
            }
        }
    }
}

/// Validates an access token. Returns `Ok(None)` when Twitch responds with 401/invalid.
pub async fn validate_access_token(
    http: &Client,
    access_token: &str,
) -> Result<Option<TokenValidation>> {
    barbed::native::validate_access_token(http, access_token).await
}

/// Attempts to refresh an access token using its refresh token.
pub async fn refresh_access_token(
    http: &Client,
    client_id: &str,
    refresh_token: &str,
) -> Result<Option<TwitchTokenBundle>> {
    Ok(
        barbed::native::refresh_access_token(http, client_id, refresh_token)
            .await?
            .map(TwitchTokenBundle::from_barbed),
    )
}

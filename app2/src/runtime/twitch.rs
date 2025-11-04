// SPDX-License-Identifier: Apache-2.0

use super::pipeline::MessagePipeline;
use anyhow::{Context, Result, anyhow, bail};
use async_channel::Sender;
use emotes_7tv::{SevenTvCatalog, SevenTvClient};
use emotes_bttv::{BttvCatalog, BttvClient};
use emotes_ffz::{FfzCatalog, FfzClient, FfzGlobalEmoteSets};
use lupo_configuration::{SettingSource, SettingsService};
use lupo_persistence::{DeliveryKind, NewMessage, PaymentInfo};
use lupo_secrets::{SecureStoreError, delete_secret, load_secret_string, store_secret};
use messages::{EmojiResolver, EmoteResolver, ResolverRegistry};
use messages_twitch::{
    ChatSubscriptionConfig, EventSubAuthError, EventSubStreamEvent, TwitchLoginState,
    TwitchTokenBundle, TwitchTokenStorage, connect_chat_stream, fetch_user_identity,
    is_twitch_unauthorized,
    refresh_access_token as twitch_refresh_access_token,
    validate_access_token as twitch_validate_access_token,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::pin::Pin;
use std::sync::Arc;
use time::{Duration as TimeDuration, OffsetDateTime};
use tokio::runtime::Runtime;
use tokio::time::Duration;
use tracing::{debug, warn};

const DEFAULT_TWITCH_CLIENT_ID: &str = "49dpqt2hwhs0gyitcm9azaaacpjpyz";

#[derive(Clone)]
pub struct TwitchConnectorContext {
    pub settings: SettingsService,
    pub pipeline: Arc<MessagePipeline>,
}

#[derive(Clone, Default)]
pub struct TwitchConnectorConfig {
    pub channel_login: Option<String>,
}

#[derive(Clone, Debug)]
pub enum TwitchStatus {
    Idle,
    Connecting,
    Connected { channel: String },
    CredentialsMissing,
    Error(String),
    Disconnected,
}

#[derive(Clone, Debug)]
pub struct TwitchMessage {
    pub preview: String,
}

#[derive(Clone, Debug)]
pub enum TwitchEvent {
    Status(TwitchStatus),
    Message(TwitchMessage),
}

pub fn spawn_listener(
    runtime: Arc<Runtime>,
    context: TwitchConnectorContext,
    sender: Sender<TwitchEvent>,
    config: TwitchConnectorConfig,
) {
    runtime.spawn(async move {
        let status_sender = sender.clone();
        let _ = status_sender
            .send(TwitchEvent::Status(TwitchStatus::Connecting))
            .await;

        match run_event_loop(context, sender.clone(), config).await {
            Ok(()) => {
                let _ = sender
                    .send(TwitchEvent::Status(TwitchStatus::Disconnected))
                    .await;
            }
            Err(err) => {
                let _ = sender
                    .send(TwitchEvent::Status(TwitchStatus::Error(err.to_string())))
                    .await;
            }
        }
    });
}

async fn run_event_loop(
    context: TwitchConnectorContext,
    sender: Sender<TwitchEvent>,
    config: TwitchConnectorConfig,
) -> Result<()> {
    let Some((login_state, stored_tokens)) = load_stored_credentials(&context.settings).await?
    else {
        let _ = sender
            .send(TwitchEvent::Status(TwitchStatus::CredentialsMissing))
            .await;
        return Ok(());
    };

    let http = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("failed to build HTTP client")?;

    let mut tokens =
        ensure_valid_access_token(&context.settings, &http, &login_state, &stored_tokens).await?;

    let broadcaster_login = config
        .channel_login
        .clone()
        .unwrap_or_else(|| login_state.login.clone())
        .to_lowercase();

    let broadcaster =
        match fetch_user_identity(
            &http,
            DEFAULT_TWITCH_CLIENT_ID,
            &tokens.access_token,
            &broadcaster_login,
        )
        .await
        {
            Ok(identity) => identity,
            Err(err) if is_twitch_unauthorized(&err) => {
                tokens = refresh_tokens_after_unauthorized(
                    &context.settings,
                    &http,
                    &login_state,
                    &tokens,
                    "fetching broadcaster identity",
                )
                .await?;
                fetch_user_identity(
                    &http,
                    DEFAULT_TWITCH_CLIENT_ID,
                    &tokens.access_token,
                    &broadcaster_login,
                )
                .await?
            }
            Err(err) => return Err(err),
        };

    let mut resolver_registry = ResolverRegistry::new();
    resolver_registry.add_global(EmojiResolver::shared());

    let seventv = build_seventv_resolvers(&broadcaster.id, &broadcaster.login).await;
    if let Some(global) = seventv.global {
        resolver_registry.add_global(global);
    }
    if let Some(channel) = seventv.channel {
        resolver_registry.add_channel(broadcaster.id.clone(), channel);
    }

    let bttv = build_bttv_resolvers(&broadcaster.id, &broadcaster.login).await;
    if let Some(global) = bttv.global {
        resolver_registry.add_global(global);
    }
    if let Some(channel) = bttv.channel {
        resolver_registry.add_channel(broadcaster.id.clone(), channel);
    }

    let ffz = build_ffz_resolvers(&broadcaster.id, &broadcaster.login).await;
    if let Some(global) = ffz.global {
        resolver_registry.add_global(global);
    }
    if let Some(channel) = ffz.channel {
        resolver_registry.add_channel(broadcaster.id.clone(), channel);
    }

    let mut subscription_config = ChatSubscriptionConfig {
        client_id: DEFAULT_TWITCH_CLIENT_ID.to_string(),
        access_token: tokens.access_token.clone(),
        broadcaster_id: broadcaster.id.clone(),
        user_id: login_state.user_id.clone(),
        emote_resolver: Some(Arc::new(resolver_registry)),
    };

    let mut stream = match connect_chat_stream(&http, subscription_config.clone()).await {
        Ok(stream) => stream,
        Err(err) if err.downcast_ref::<EventSubAuthError>().is_some() => {
            tokens = refresh_tokens_after_unauthorized(
                &context.settings,
                &http,
                &login_state,
                &tokens,
                "creating EventSub chat subscription",
            )
            .await?;
            subscription_config.access_token = tokens.access_token.clone();
            connect_chat_stream(&http, subscription_config.clone()).await?
        }
        Err(err) => return Err(err),
    };

    let _ = sender
        .send(TwitchEvent::Status(TwitchStatus::Connected {
            channel: broadcaster.display_name.clone(),
        }))
        .await;

    loop {
        match stream.next_event().await? {
            Some(EventSubStreamEvent::ChatMessage(message)) => {
                handle_chat_message(&context, &sender, message).await?;
            }
            Some(EventSubStreamEvent::MessageDeleted(deleted)) => {
                handle_message_deleted(&context, deleted).await?;
            }
            Some(EventSubStreamEvent::Keepalive) => {
                debug!("received EventSub keepalive");
            }
            Some(EventSubStreamEvent::SessionReconnect { reconnect_url }) => {
                warn!(%reconnect_url, "Twitch requested EventSub reconnect");
                break;
            }
            Some(EventSubStreamEvent::SessionDisconnect { status, reason }) => {
                warn!(%status, ?reason, "EventSub session disconnected by Twitch");
                break;
            }
            Some(EventSubStreamEvent::Revocation { status, reason }) => {
                bail!(
                    "EventSub subscription was revoked (status {:?}, reason {:?})",
                    status,
                    reason
                );
            }
            None => {
                warn!("EventSub stream ended by Twitch");
                break;
            }
        }
    }

    Ok(())
}

async fn handle_chat_message(
    context: &TwitchConnectorContext,
    sender: &Sender<TwitchEvent>,
    message: messages_twitch::ChatMessage,
) -> Result<()> {
    let spoken_text = message.body.text_only();
    let display_text = message.body.raw_text.clone();
    let sender_name = if message.chatter_display_name.is_empty() {
        message.chatter_login.clone()
    } else {
        message.chatter_display_name.clone()
    };

    let received_at = OffsetDateTime::now_utc();
    let is_moderator = has_badge(&message, "moderator");
    let is_channel_owner = has_badge(&message, "broadcaster");
    let is_provider_bot = has_badge(&message, "bot");
    let badges = message
        .badges
        .iter()
        .map(|badge| format!("{}:{}", badge.set_id, badge.id))
        .collect::<Vec<_>>();

    let mut request = NewMessage::new("twitch_chat", sender_name, spoken_text, received_at)
        .with_provider_message_id(message.message_id.clone())
        .with_channel_id(message.broadcaster_id.clone())
        .with_user_id(message.chatter_user_id.clone())
        .with_metadata("chatter_login", json!(message.chatter_login))
        .with_metadata("broadcaster_login", json!(message.broadcaster_login))
        .with_metadata("is_moderator", json!(is_moderator))
        .with_metadata("is_channel_owner", json!(is_channel_owner))
        .with_metadata("provider_bot", json!(is_provider_bot))
        .with_metadata("badges", json!(badges));

    if let Some(color) = &message.color {
        request = request.with_metadata("color", json!(color));
    }
    if let Some(source_timestamp) = message.source_timestamp {
        request = request.with_source_timestamp(source_timestamp);
    }
    if let Some(bits) = message.bits.filter(|bits| *bits > 0) {
        request = request
            .with_delivery_kind(DeliveryKind::Paid)
            .with_payment(PaymentInfo {
                amount_minor_units: bits,
                currency_code: "bits".to_string(),
                provider_tier_code: Some(bits.to_string()),
                provider_color_rank: None,
            });
    }

    context
        .pipeline
        .submit(request)
        .await
        .map_err(|err| anyhow!(err))?;

    let _ = sender
        .send(TwitchEvent::Message(TwitchMessage {
            preview: display_text,
        }))
        .await;
    Ok(())
}

async fn handle_message_deleted(
    context: &TwitchConnectorContext,
    deleted: messages_twitch::ChatMessageDeleted,
) -> Result<()> {
    context
        .pipeline
        .mark_moderator_deleted("twitch_chat", &deleted.message_id)
        .await
        .map_err(|err| anyhow!(err))
}

fn has_badge(message: &messages_twitch::ChatMessage, badge_set_id: &str) -> bool {
    message
        .badges
        .iter()
        .any(|badge| badge.set_id == badge_set_id)
}

#[derive(Default)]
struct ProviderResolvers {
    global: Option<Arc<dyn EmoteResolver>>,
    channel: Option<Arc<dyn EmoteResolver>>,
}

async fn build_seventv_resolvers(channel_id: &str, channel_login: &str) -> ProviderResolvers {
    let client = match SevenTvClient::new() {
        Ok(client) => client,
        Err(err) => {
            warn!(?err, "failed to initialise 7TV client");
            return ProviderResolvers::default();
        }
    };

    let mut catalog = SevenTvCatalog::new();

    match client.fetch_global_emotes().await {
        Ok(set) => catalog.insert_global_set(&set),
        Err(err) => warn!(?err, "failed to fetch 7TV global emotes"),
    }

    match client.fetch_channel_emotes_by_twitch_id(channel_id).await {
        Ok(set) => catalog.insert_channel_set(channel_id.to_string(), &set),
        Err(err) => warn!(
            ?err,
            channel = channel_login,
            "failed to fetch 7TV channel emotes"
        ),
    }

    if catalog.is_empty() {
        ProviderResolvers::default()
    } else {
        ProviderResolvers {
            global: catalog.global_resolver(),
            channel: catalog.channel_resolver(channel_id),
        }
    }
}

async fn build_bttv_resolvers(channel_id: &str, channel_login: &str) -> ProviderResolvers {
    let client = match BttvClient::new() {
        Ok(client) => client,
        Err(err) => {
            warn!(?err, "failed to initialise BetterTTV client");
            return ProviderResolvers::default();
        }
    };

    let mut catalog = BttvCatalog::new();

    match client.fetch_global_emotes().await {
        Ok(set) => {
            catalog.insert_global_emotes(set.emotes.iter().cloned());
        }
        Err(err) => warn!(?err, "failed to fetch BetterTTV global emotes"),
    }

    match client.fetch_channel_emotes_by_twitch_id(channel_id).await {
        Ok(sets) => {
            let emotes = sets.all_emotes();
            if !emotes.is_empty() {
                catalog.insert_channel_emotes(channel_id.to_string(), emotes);
            }
        }
        Err(err) => warn!(
            ?err,
            channel = channel_login,
            "failed to fetch BetterTTV channel emotes"
        ),
    }

    if catalog.is_empty() {
        ProviderResolvers::default()
    } else {
        ProviderResolvers {
            global: catalog.global_resolver(),
            channel: catalog.channel_resolver(channel_id),
        }
    }
}

async fn build_ffz_resolvers(channel_id: &str, channel_login: &str) -> ProviderResolvers {
    let client = match FfzClient::new() {
        Ok(client) => client,
        Err(err) => {
            warn!(?err, "failed to initialise FrankerFaceZ client");
            return ProviderResolvers::default();
        }
    };

    let mut catalog = FfzCatalog::new();

    match client.fetch_global_emotes().await {
        Ok(sets) => {
            insert_ffz_global_sets(channel_id, &mut catalog, &sets);
        }
        Err(err) => warn!(?err, "failed to fetch FFZ global emotes"),
    }

    match client.fetch_room_emotes_by_twitch_id(channel_id).await {
        Ok(room_sets) => {
            catalog.insert_channel_sets(channel_id.to_string(), room_sets.sets.iter());
        }
        Err(err) => warn!(
            ?err,
            channel = channel_login,
            "failed to fetch FFZ channel emotes"
        ),
    }

    if catalog.is_empty() {
        ProviderResolvers::default()
    } else {
        ProviderResolvers {
            global: catalog.global_resolver(),
            channel: catalog.channel_resolver(channel_id),
        }
    }
}

fn insert_ffz_global_sets(channel_id: &str, catalog: &mut FfzCatalog, sets: &FfzGlobalEmoteSets) {
    let default_sets = sets.default_sets();
    if !default_sets.is_empty() {
        catalog.insert_global_sets(default_sets.clone());
    }

    let user_sets = sets.sets_for_user(channel_id);
    if !user_sets.is_empty() {
        catalog.insert_channel_sets(channel_id.to_string(), user_sets.clone());
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
    fn from_parts(state: &TwitchLoginState, tokens: &TwitchTokenBundle) -> Self {
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
    fn from_parts(state: &TwitchLoginState, tokens: &TwitchTokenBundle) -> Self {
        Self {
            login_state: state.clone(),
            token_info: StoredTokenInfo::from_parts(state, tokens),
        }
    }
}

pub async fn load_stored_credentials(
    service: &SettingsService,
) -> Result<Option<(TwitchLoginState, TwitchTokenBundle)>> {
    let storage = SettingsBackedTokenStorage::new(service);
    storage.load_tokens().await
}

async fn ensure_valid_access_token(
    service: &SettingsService,
    http: &Client,
    state: &TwitchLoginState,
    tokens: &TwitchTokenBundle,
) -> Result<TwitchTokenBundle> {
    if let Some(validation) = twitch_validate_access_token(http, &tokens.access_token)
        .await
        .context("failed to validate stored Twitch token")?
    {
        debug!(
            expires_in = validation.expires_in,
            "Twitch access token validated successfully"
        );
        return Ok(tokens.clone());
    }

    debug!("Stored Twitch access token rejected; attempting refresh");
    refresh_access_token_and_store(service, http, state, tokens).await
}

async fn refresh_tokens_after_unauthorized(
    service: &SettingsService,
    http: &Client,
    state: &TwitchLoginState,
    tokens: &TwitchTokenBundle,
    context: &str,
) -> Result<TwitchTokenBundle> {
    match twitch_validate_access_token(http, &tokens.access_token)
        .await
        .with_context(|| format!("failed to validate Twitch token after {context} returned 401"))?
    {
        Some(validation) => {
            debug!(
                expires_in = validation.expires_in,
                reason = context,
                "Twitch token reported valid despite HTTP 401"
            );
        }
        None => {
            debug!(
                reason = context,
                "Twitch token confirmed invalid after HTTP 401"
            );
        }
    }

    refresh_access_token_and_store(service, http, state, tokens).await
}

async fn refresh_access_token_and_store(
    service: &SettingsService,
    http: &Client,
    state: &TwitchLoginState,
    tokens: &TwitchTokenBundle,
) -> Result<TwitchTokenBundle> {
    let refresh_token = tokens.refresh_token.as_deref().ok_or_else(|| {
        anyhow!("Stored Twitch login cannot be refreshed; re-run Twitch authentication")
    })?;

    let Some(new_tokens) =
        twitch_refresh_access_token(http, DEFAULT_TWITCH_CLIENT_ID, refresh_token)
            .await
            .context("failed to refresh Twitch access token")?
    else {
        bail!("Twitch rejected the stored refresh token; re-run Twitch authentication.");
    };

    let storage = SettingsBackedTokenStorage::new(service);
    storage
        .store_tokens(state, &new_tokens)
        .await
        .context("failed to persist refreshed Twitch tokens")?;

    Ok(new_tokens)
}

struct SettingsBackedTokenStorage {
    service: SettingsService,
}

impl SettingsBackedTokenStorage {
    fn new(service: &SettingsService) -> Self {
        Self {
            service: service.clone(),
        }
    }

    fn secure_error(err: SecureStoreError) -> anyhow::Error {
        anyhow!(err.to_string())
    }
}

const SECRET_TWITCH_ACCESS_KEY: &str = "secret_twitch_access_token";
const SECRET_TWITCH_REFRESH_KEY: &str = "secret_twitch_refresh_token";
const TWITCH_AUTH_STATE_KEY: &str = "twitch_auth_state";

impl TwitchTokenStorage for SettingsBackedTokenStorage {
    fn store_tokens<'s>(
        &'s self,
        state: &'s TwitchLoginState,
        tokens: &'s TwitchTokenBundle,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 's>> {
        let service = self.service.clone();
        Box::pin(async move {
            if tokens.access_token.is_empty() {
                bail!("Twitch returned an empty access token");
            }

            let metadata = StoredAuthState::from_parts(state, tokens);
            let value = serde_json::to_value(&metadata)
                .map_err(|err| anyhow!("failed to serialise Twitch auth metadata: {err}"))?;

            store_secret(
                &service,
                SECRET_TWITCH_ACCESS_KEY,
                tokens.access_token.as_bytes(),
            )
            .map_err(Self::secure_error)?;

            if let Some(refresh) = tokens.refresh_token.as_ref() {
                store_secret(&service, SECRET_TWITCH_REFRESH_KEY, refresh.as_bytes())
                    .map_err(Self::secure_error)?;
            } else {
                delete_secret(&service, SECRET_TWITCH_REFRESH_KEY).map_err(Self::secure_error)?;
            }

            service
                .set(TWITCH_AUTH_STATE_KEY, value)
                .map_err(|err| anyhow!(err.to_string()))?;

            Ok(())
        })
    }

    fn load_tokens(
        &self,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<(TwitchLoginState, TwitchTokenBundle)>>>
                + Send
                + '_,
        >,
    > {
        let service = self.service.clone();
        Box::pin(async move {
            let Some(metadata) = read_metadata(&service)? else {
                return Ok(None);
            };

            let access_token = load_secret_string(&service, SECRET_TWITCH_ACCESS_KEY)
                .map_err(Self::secure_error)?;
            let Some(access_token) = access_token else {
                return Ok(None);
            };

            let refresh_token = load_secret_string(&service, SECRET_TWITCH_REFRESH_KEY)
                .map_err(Self::secure_error)?;

            let tokens = TwitchTokenBundle {
                access_token,
                refresh_token,
                expires_in: metadata.token_info.expires_in,
                scope: metadata.login_state.scopes.clone(),
                token_type: metadata.token_info.token_type.clone(),
            };

            Ok(Some((metadata.login_state, tokens)))
        })
    }

    fn clear_tokens(&self) -> Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        let service = self.service.clone();
        Box::pin(async move {
            delete_secret(&service, SECRET_TWITCH_ACCESS_KEY).map_err(Self::secure_error)?;
            delete_secret(&service, SECRET_TWITCH_REFRESH_KEY).map_err(Self::secure_error)?;
            service
                .reset(TWITCH_AUTH_STATE_KEY)
                .map_err(|err| anyhow!(err.to_string()))?;
            Ok(())
        })
    }
}

fn read_metadata(service: &SettingsService) -> Result<Option<StoredAuthState>> {
    let record = service
        .get(TWITCH_AUTH_STATE_KEY)
        .map_err(|err| anyhow!(err.to_string()))?;
    if matches!(record.source, SettingSource::Default) {
        return Ok(None);
    }

    if record
        .value
        .as_object()
        .map(|object| object.is_empty())
        .unwrap_or(false)
    {
        return Ok(None);
    }

    let metadata: StoredAuthState = serde_json::from_value(record.value)
        .map_err(|err| anyhow!("invalid Twitch auth metadata: {err}"))?;
    Ok(Some(metadata))
}

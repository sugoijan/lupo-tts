// SPDX-License-Identifier: Apache-2.0

use super::pipeline::MessagePipeline;
use anyhow::{Context, Result, anyhow, bail};
use async_channel::Sender;
use lupo_configuration::SettingsService;
use lupo_persistence::{DeliveryKind, NewMessage, PaymentInfo};
use lupo_secrets::load_secret_string;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::runtime::Runtime;
use tokio::time::{Duration, sleep};

const YOUTUBE_API_KEY_SECRET_KEY: &str = "secret_youtube_api_key";
const DEFAULT_POLL_INTERVAL_MS: u64 = 5_000;
const MAX_POLL_INTERVAL_MS: u64 = 60_000;

#[derive(Clone)]
pub struct YouTubeConnectorContext {
    pub settings: SettingsService,
    pub pipeline: Arc<MessagePipeline>,
}

#[derive(Clone, Default)]
pub struct YouTubeConnectorConfig {
    pub live_chat_id: Option<String>,
}

#[derive(Clone, Debug)]
pub enum YouTubeStatus {
    Idle,
    Connecting,
    Connected { live_chat_id: String },
    CredentialsMissing,
    Error(String),
    Disconnected,
}

#[derive(Clone, Debug)]
pub struct YouTubeMessage {
    pub preview: String,
}

#[derive(Clone, Debug)]
pub enum YouTubeEvent {
    Status(YouTubeStatus),
    Message(YouTubeMessage),
}

pub fn spawn_listener(
    runtime: Arc<Runtime>,
    context: YouTubeConnectorContext,
    sender: Sender<YouTubeEvent>,
    config: YouTubeConnectorConfig,
) -> tokio::task::JoinHandle<()> {
    runtime.spawn(async move {
        let _ = sender
            .send(YouTubeEvent::Status(YouTubeStatus::Connecting))
            .await;

        match run_event_loop(context, sender.clone(), config).await {
            Ok(()) => {
                let _ = sender
                    .send(YouTubeEvent::Status(YouTubeStatus::Disconnected))
                    .await;
            }
            Err(err) => {
                let _ = sender
                    .send(YouTubeEvent::Status(YouTubeStatus::Error(err.to_string())))
                    .await;
            }
        }
    })
}

async fn run_event_loop(
    context: YouTubeConnectorContext,
    sender: Sender<YouTubeEvent>,
    config: YouTubeConnectorConfig,
) -> Result<()> {
    let api_key = match load_secret_string(&context.settings, YOUTUBE_API_KEY_SECRET_KEY)
        .context("failed to load YouTube API key")?
    {
        Some(key) if !key.trim().is_empty() => key,
        _ => {
            let _ = sender
                .send(YouTubeEvent::Status(YouTubeStatus::CredentialsMissing))
                .await;
            return Ok(());
        }
    };

    let Some(live_chat_id) = resolve_live_chat_id(&context.settings, config.live_chat_id)? else {
        let _ = sender
            .send(YouTubeEvent::Status(YouTubeStatus::CredentialsMissing))
            .await;
        return Ok(());
    };

    let http = Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("failed to build YouTube HTTP client")?;

    let _ = sender
        .send(YouTubeEvent::Status(YouTubeStatus::Connected {
            live_chat_id: live_chat_id.clone(),
        }))
        .await;

    let mut page_token: Option<String> = None;
    loop {
        let response = fetch_live_chat_page(&http, &api_key, &live_chat_id, page_token.as_deref())
            .await
            .with_context(|| {
                format!("failed to fetch YouTube live chat page for {live_chat_id}")
            })?;

        for item in response.items {
            let Some(normalized) = normalize_live_chat_item(item, OffsetDateTime::now_utc()) else {
                continue;
            };

            context
                .pipeline
                .submit(normalized.request)
                .await
                .map_err(|err| anyhow!(err))?;

            let _ = sender
                .send(YouTubeEvent::Message(YouTubeMessage {
                    preview: normalized.preview,
                }))
                .await;
        }

        page_token = response.next_page_token;

        let sleep_ms = response
            .polling_interval_millis
            .unwrap_or(DEFAULT_POLL_INTERVAL_MS)
            .clamp(1_000, MAX_POLL_INTERVAL_MS);
        sleep(Duration::from_millis(sleep_ms)).await;
    }
}

fn resolve_live_chat_id(
    settings: &SettingsService,
    config_live_chat_id: Option<String>,
) -> Result<Option<String>> {
    if let Some(value) = config_live_chat_id {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(Some(trimmed.to_string()));
        }
    }

    let record = settings
        .get("connections")
        .map_err(|err| anyhow!(err.to_string()))?;
    let Some(value) = record.value.get("youtube_live_chat_id") else {
        return Ok(None);
    };
    let Some(raw) = value.as_str() else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

async fn fetch_live_chat_page(
    client: &Client,
    api_key: &str,
    live_chat_id: &str,
    page_token: Option<&str>,
) -> Result<YouTubeLiveChatResponse> {
    let mut request = client
        .get("https://www.googleapis.com/youtube/v3/liveChat/messages")
        .query(&[
            ("part", "id,snippet,authorDetails"),
            ("liveChatId", live_chat_id),
            ("maxResults", "200"),
            ("key", api_key),
        ]);

    if let Some(token) = page_token {
        request = request.query(&[("pageToken", token)]);
    }

    let response = request.send().await.context("request failed")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("YouTube API returned {status}: {body}");
    }

    response
        .json::<YouTubeLiveChatResponse>()
        .await
        .context("failed to parse YouTube response")
}

fn parse_rfc3339_timestamp(value: Option<&str>) -> Option<OffsetDateTime> {
    value.and_then(|raw| OffsetDateTime::parse(raw, &Rfc3339).ok())
}

#[derive(Debug)]
struct NormalizedYouTubeMessage {
    request: NewMessage,
    preview: String,
}

fn normalize_live_chat_item(
    item: YouTubeLiveChatMessageItem,
    received_at: OffsetDateTime,
) -> Option<NormalizedYouTubeMessage> {
    let YouTubeLiveChatMessageItem {
        id,
        snippet,
        author_details,
    } = item;
    let YouTubeSnippet {
        live_chat_id,
        published_at,
        display_message,
        super_chat_details,
    } = snippet;
    let YouTubeAuthorDetails {
        channel_id,
        display_name,
        is_chat_owner,
        is_chat_moderator,
    } = author_details;

    let message_text = display_message.unwrap_or_default().trim().to_string();
    if message_text.is_empty() {
        return None;
    }

    let sender_name = display_name.unwrap_or_else(|| "YouTube".to_string());

    let mut request = NewMessage::new(
        "youtube_live_chat",
        sender_name,
        message_text.clone(),
        received_at,
    )
    .with_provider_message_id(id)
    .with_channel_id(live_chat_id)
    .with_metadata(
        "is_moderator",
        Value::Bool(is_chat_moderator.unwrap_or(false)),
    )
    .with_metadata(
        "is_channel_owner",
        Value::Bool(is_chat_owner.unwrap_or(false)),
    )
    .with_metadata("provider_bot", Value::Bool(false));

    if let Some(channel_id) = channel_id {
        request = request.with_user_id(channel_id);
    }

    if let Some(at) = parse_rfc3339_timestamp(published_at.as_deref()) {
        request = request.with_source_timestamp(at);
    }

    if let Some(super_chat) = super_chat_details {
        let amount_minor_units = super_chat.amount_micros.saturating_div(10_000);
        request = request
            .with_delivery_kind(DeliveryKind::Paid)
            .with_payment(PaymentInfo {
                amount_minor_units,
                currency_code: super_chat.currency,
                provider_tier_code: super_chat.tier.as_ref().map(ToString::to_string),
                provider_color_rank: None,
            });
    }

    Some(NormalizedYouTubeMessage {
        request,
        preview: message_text,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YouTubeLiveChatResponse {
    #[serde(default)]
    items: Vec<YouTubeLiveChatMessageItem>,
    #[serde(default)]
    next_page_token: Option<String>,
    #[serde(default)]
    polling_interval_millis: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YouTubeLiveChatMessageItem {
    id: String,
    snippet: YouTubeSnippet,
    author_details: YouTubeAuthorDetails,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YouTubeSnippet {
    live_chat_id: String,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    display_message: Option<String>,
    #[serde(default)]
    super_chat_details: Option<YouTubeSuperChatDetails>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YouTubeAuthorDetails {
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    is_chat_owner: Option<bool>,
    #[serde(default)]
    is_chat_moderator: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YouTubeSuperChatDetails {
    #[serde(default)]
    amount_micros: u64,
    currency: String,
    #[serde(default)]
    tier: Option<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use lupo_persistence::DeliveryKind;
    use serde_json::json;

    #[test]
    fn normalize_live_chat_item_maps_fields_and_superchat() {
        let received_at =
            OffsetDateTime::parse("2026-02-23T12:00:00Z", &Rfc3339).expect("valid timestamp");
        let item = YouTubeLiveChatMessageItem {
            id: "message-123".to_string(),
            snippet: YouTubeSnippet {
                live_chat_id: "chat-abc".to_string(),
                published_at: Some("2026-02-23T11:59:30Z".to_string()),
                display_message: Some("Hello from YouTube".to_string()),
                super_chat_details: Some(YouTubeSuperChatDetails {
                    amount_micros: 2_500_000,
                    currency: "USD".to_string(),
                    tier: Some(4),
                }),
            },
            author_details: YouTubeAuthorDetails {
                channel_id: Some("UC_TEST".to_string()),
                display_name: Some("Streamer".to_string()),
                is_chat_owner: Some(true),
                is_chat_moderator: Some(true),
            },
        };

        let normalized = normalize_live_chat_item(item, received_at).expect("message expected");
        assert_eq!(normalized.preview, "Hello from YouTube");
        assert_eq!(normalized.request.provider, "youtube_live_chat");
        assert_eq!(
            normalized.request.provider_message_id.as_deref(),
            Some("message-123")
        );
        assert_eq!(normalized.request.channel_id.as_deref(), Some("chat-abc"));
        assert_eq!(normalized.request.user_id.as_deref(), Some("UC_TEST"));
        assert_eq!(normalized.request.sender, "Streamer");
        assert_eq!(normalized.request.content, "Hello from YouTube");
        assert_eq!(normalized.request.received_at, received_at);
        assert_eq!(normalized.request.delivery_kind, DeliveryKind::Paid);

        let payment = normalized.request.payment.expect("payment expected");
        assert_eq!(payment.amount_minor_units, 250);
        assert_eq!(payment.currency_code, "USD");
        assert_eq!(payment.provider_tier_code.as_deref(), Some("4"));

        assert_eq!(
            normalized
                .request
                .metadata
                .get("is_channel_owner")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            normalized
                .request
                .metadata
                .get("is_moderator")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            normalized
                .request
                .source_timestamp
                .expect("source timestamp expected"),
            OffsetDateTime::parse("2026-02-23T11:59:30Z", &Rfc3339).expect("valid timestamp")
        );
    }

    #[test]
    fn normalize_live_chat_item_skips_empty_messages() {
        let item = YouTubeLiveChatMessageItem {
            id: "message-123".to_string(),
            snippet: YouTubeSnippet {
                live_chat_id: "chat-abc".to_string(),
                published_at: None,
                display_message: Some("   ".to_string()),
                super_chat_details: None,
            },
            author_details: YouTubeAuthorDetails {
                channel_id: None,
                display_name: Some("Streamer".to_string()),
                is_chat_owner: Some(false),
                is_chat_moderator: Some(false),
            },
        };

        let result = normalize_live_chat_item(item, OffsetDateTime::now_utc());
        assert!(result.is_none());
    }

    #[test]
    fn youtube_live_chat_response_deserializes_camel_case_fields() {
        let payload = json!({
            "pollingIntervalMillis": 5000,
            "nextPageToken": "NEXT",
            "items": [
                {
                    "id": "msg-1",
                    "snippet": {
                        "liveChatId": "chat-1",
                        "publishedAt": "2026-02-23T12:00:00Z",
                        "displayMessage": "Hi"
                    },
                    "authorDetails": {
                        "channelId": "UC1",
                        "displayName": "Alice",
                        "isChatOwner": false,
                        "isChatModerator": true
                    }
                }
            ]
        });

        let response: YouTubeLiveChatResponse =
            serde_json::from_value(payload).expect("response should deserialize");
        assert_eq!(response.polling_interval_millis, Some(5_000));
        assert_eq!(response.next_page_token.as_deref(), Some("NEXT"));
        assert_eq!(response.items.len(), 1);
        assert_eq!(response.items[0].snippet.live_chat_id, "chat-1");
    }
}

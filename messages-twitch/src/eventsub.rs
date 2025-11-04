// SPDX-License-Identifier: Apache-2.0
//! LUPO-facing EventSub helpers backed by `barbed`.

use anyhow::Result;
use barbed::eventsub::{
    EventSubChatBadge as BarbedChatBadge, EventSubChatMessage as BarbedChatMessage,
    EventSubChatMessageDeleted as BarbedChatMessageDeleted,
    EventSubMessageEmote as BarbedMessageEmote,
};
use messages::{
    EmoteId, EmoteImage, EmoteImageFormat, EmoteImageScale, EmoteProvider, EmoteRef,
    EmoteThemeMode, Message, MessageSegment, ResolverRegistry, TextRange,
};
use reqwest::Client;
use std::sync::Arc;
use time::OffsetDateTime;

pub use barbed::native::EventSubAuthError;

/// Connection details used to create EventSub chat subscriptions.
#[derive(Clone)]
pub struct ChatSubscriptionConfig {
    pub client_id: String,
    pub access_token: String,
    pub broadcaster_id: String,
    pub user_id: String,
    pub emote_resolver: Option<Arc<ResolverRegistry>>,
}

/// High-level stream events surfaced to callers.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum EventSubStreamEvent {
    ChatMessage(ChatMessage),
    MessageDeleted(ChatMessageDeleted),
    Keepalive,
    SessionReconnect {
        reconnect_url: String,
    },
    SessionDisconnect {
        status: String,
        reason: Option<String>,
    },
    Revocation {
        status: Option<String>,
        reason: Option<String>,
    },
}

/// Simplified chat message payload.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub message_id: String,
    pub broadcaster_id: String,
    pub broadcaster_login: String,
    pub broadcaster_display_name: String,
    pub chatter_user_id: String,
    pub chatter_login: String,
    pub chatter_display_name: String,
    pub source_timestamp: Option<OffsetDateTime>,
    pub body: Message,
    pub badges: Vec<ChatBadge>,
    pub color: Option<String>,
    pub bits: Option<u64>,
}

impl From<BarbedChatMessage> for ChatMessage {
    fn from(value: BarbedChatMessage) -> Self {
        let bits = value.cheer.as_ref().map(|cheer| cheer.bits);
        Self {
            message_id: value.message_id,
            broadcaster_id: value.broadcaster_user_id,
            broadcaster_login: value.broadcaster_user_login,
            broadcaster_display_name: value.broadcaster_user_name,
            chatter_user_id: value.chatter_user_id,
            chatter_login: value.chatter_user_login,
            chatter_display_name: value.chatter_user_name,
            source_timestamp: value.source_timestamp,
            body: build_message_from_fragments(value.message),
            badges: value.badges.into_iter().map(ChatBadge::from).collect(),
            color: value.color,
            bits,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatMessageDeleted {
    pub message_id: String,
    pub broadcaster_id: String,
    pub target_user_id: Option<String>,
    pub source_timestamp: Option<OffsetDateTime>,
}

impl From<BarbedChatMessageDeleted> for ChatMessageDeleted {
    fn from(value: BarbedChatMessageDeleted) -> Self {
        Self {
            message_id: value.message_id,
            broadcaster_id: value.broadcaster_user_id,
            target_user_id: value.target_user_id,
            source_timestamp: value.source_timestamp,
        }
    }
}

/// Single badge reference attached to a chat message.
#[derive(Debug, Clone)]
pub struct ChatBadge {
    pub set_id: String,
    pub id: String,
    pub info: Option<String>,
}

impl From<BarbedChatBadge> for ChatBadge {
    fn from(value: BarbedChatBadge) -> Self {
        Self {
            set_id: value.set_id,
            id: value.id,
            info: value.info,
        }
    }
}

/// Streaming handle that yields EventSub chat notifications.
pub struct EventSubChatStream {
    inner: barbed::native::EventSubChatStream,
    emote_resolver: Option<Arc<ResolverRegistry>>,
}

impl EventSubChatStream {
    pub fn session_id(&self) -> &str {
        self.inner.session_id()
    }

    pub async fn next_event(&mut self) -> Result<Option<EventSubStreamEvent>> {
        let Some(event) = self.inner.next_event().await? else {
            return Ok(None);
        };

        let mapped = match event {
            barbed::eventsub::EventSubStreamEvent::ChatMessage(message) => {
                let mut message = ChatMessage::from(message);
                if let Some(resolver) = &self.emote_resolver {
                    message
                        .body
                        .resolve_emotes(&message.broadcaster_id, resolver.as_ref());
                }
                EventSubStreamEvent::ChatMessage(message)
            }
            barbed::eventsub::EventSubStreamEvent::MessageDeleted(deleted) => {
                EventSubStreamEvent::MessageDeleted(ChatMessageDeleted::from(deleted))
            }
            barbed::eventsub::EventSubStreamEvent::Keepalive => EventSubStreamEvent::Keepalive,
            barbed::eventsub::EventSubStreamEvent::SessionReconnect { reconnect_url } => {
                EventSubStreamEvent::SessionReconnect { reconnect_url }
            }
            barbed::eventsub::EventSubStreamEvent::SessionDisconnect { status, reason } => {
                EventSubStreamEvent::SessionDisconnect { status, reason }
            }
            barbed::eventsub::EventSubStreamEvent::Revocation { status, reason } => {
                EventSubStreamEvent::Revocation { status, reason }
            }
        };

        Ok(Some(mapped))
    }

    pub async fn close(&mut self) -> Result<()> {
        self.inner.close().await
    }
}

/// Connects to EventSub, waits for the welcome message, and issues required chat subscriptions.
pub async fn connect_chat_stream(
    http: &Client,
    config: ChatSubscriptionConfig,
) -> Result<EventSubChatStream> {
    let inner = barbed::native::connect_chat_stream(
        http,
        barbed::native::ChatSubscriptionConfig {
            client_id: config.client_id.clone(),
            access_token: config.access_token.clone(),
            broadcaster_id: config.broadcaster_id.clone(),
            user_id: config.user_id.clone(),
        },
    )
    .await?;

    Ok(EventSubChatStream {
        inner,
        emote_resolver: config.emote_resolver,
    })
}

fn build_message_from_fragments(body: barbed::eventsub::EventSubChatMessageText) -> Message {
    let raw_text = body.text;
    let fragments = body.fragments;
    let mut message = Message::new(raw_text);

    if fragments.is_empty() {
        if !message.raw_text.is_empty() {
            let len = message.raw_text.len();
            message.push_segment(MessageSegment::Text { range: 0..len });
        }
        return message;
    }

    let mut cursor: usize = 0;
    let total_len = message.raw_text.len();

    for fragment in fragments {
        let fragment_bytes = fragment.text.len();
        let start = cursor.min(total_len);
        let mut end = start.saturating_add(fragment_bytes);
        if end > total_len {
            end = total_len;
        }
        let range: TextRange = start..end;

        if let Some(emote) = fragment.emote {
            let emote_ref = build_twitch_emote(&fragment.text, &emote);
            message.push_segment(MessageSegment::Emote {
                range: range.clone(),
                emote: Arc::new(emote_ref),
                modifiers: Vec::new(),
            });
        } else {
            message.push_segment(MessageSegment::Text {
                range: range.clone(),
            });
        }

        cursor = end;
    }

    if cursor < total_len {
        message.push_segment(MessageSegment::Text {
            range: cursor..total_len,
        });
    }

    message
}

fn build_twitch_emote(code: &str, emote: &BarbedMessageEmote) -> EmoteRef {
    let formats: Vec<EmoteImageFormat> = if emote.format.is_empty() {
        vec![EmoteImageFormat::Static]
    } else {
        emote
            .format
            .iter()
            .map(|value| EmoteImageFormat::parse(value))
            .collect()
    };
    let themes: Vec<EmoteThemeMode> = if emote.theme_modes.is_empty() {
        vec![EmoteThemeMode::Light, EmoteThemeMode::Dark]
    } else {
        emote
            .theme_modes
            .iter()
            .map(|value| EmoteThemeMode::parse(value))
            .collect()
    };
    let scales: Vec<EmoteImageScale> = if emote.scale.is_empty() {
        vec![EmoteImageScale::One]
    } else {
        emote
            .scale
            .iter()
            .map(|value| EmoteImageScale::parse(value))
            .collect()
    };

    let mut images = Vec::new();
    for format in &formats {
        for theme in &themes {
            for scale in &scales {
                images.push(EmoteImage {
                    format: format.clone(),
                    theme_mode: theme.clone(),
                    scale: scale.clone(),
                    url: format!(
                        "https://static-cdn.jtvnw.net/emoticons/v2/{}/{}/{}/{}",
                        emote.id,
                        format.as_str(),
                        theme.as_str(),
                        scale.as_str()
                    ),
                });
            }
        }
    }

    let is_animated = formats
        .iter()
        .any(|format| matches!(format, EmoteImageFormat::Animated));

    EmoteRef::new(
        EmoteId::new(EmoteProvider::Twitch, emote.id.clone()),
        code,
        is_animated,
        images,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn barbed_fragments_convert_to_message_segments() {
        let message = ChatMessage::from(BarbedChatMessage {
            broadcaster_user_id: "777".to_string(),
            broadcaster_user_login: "streamer".to_string(),
            broadcaster_user_name: "Streamer".to_string(),
            chatter_user_id: "42".to_string(),
            chatter_user_login: "viewer".to_string(),
            chatter_user_name: "Viewer".to_string(),
            message_id: "msg-1".to_string(),
            message: barbed::eventsub::EventSubChatMessageText {
                text: "Kappa hi".to_string(),
                fragments: vec![
                    barbed::eventsub::EventSubMessageFragment {
                        fragment_type: barbed::eventsub::EventSubMessageFragmentType::Emote,
                        text: "Kappa".to_string(),
                        emote: Some(BarbedMessageEmote {
                            id: "25".to_string(),
                            emote_set_id: None,
                            owner_id: None,
                            format: vec!["static".to_string()],
                            scale: vec!["1.0".to_string()],
                            theme_modes: vec!["light".to_string()],
                        }),
                        mention: None,
                        cheermote: None,
                    },
                    barbed::eventsub::EventSubMessageFragment {
                        fragment_type: barbed::eventsub::EventSubMessageFragmentType::Text,
                        text: " hi".to_string(),
                        emote: None,
                        mention: None,
                        cheermote: None,
                    },
                ],
            },
            cheer: Some(barbed::eventsub::EventSubCheer { bits: 100 }),
            badges: vec![BarbedChatBadge {
                set_id: "moderator".to_string(),
                id: "1".to_string(),
                info: None,
            }],
            color: Some("#ffffff".to_string()),
            source_timestamp: None,
        });

        assert_eq!(message.bits, Some(100));
        assert_eq!(message.badges.len(), 1);
        assert_eq!(message.body.raw_text, "Kappa hi");
        assert_eq!(message.body.segments.len(), 2);
        assert!(matches!(message.body.segments[0], MessageSegment::Emote { .. }));
        assert!(matches!(message.body.segments[1], MessageSegment::Text { .. }));
    }
}

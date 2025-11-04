// SPDX-License-Identifier: Apache-2.0
//! Twitch integration primitives shared across LUPO applications.

pub mod auth;
pub mod eventsub;

pub use auth::{
    StoredAuthState, StoredTokenInfo, TokenValidation, TwitchAuthClient, TwitchAuthConfig,
    TwitchAuthOutcome, TwitchDeviceAuthorization, TwitchLoginState, TwitchTokenBundle,
    TwitchTokenStorage, TwitchUnauthorizedError, TwitchUserIdentity, fetch_user_identity,
    is_twitch_unauthorized, refresh_access_token, validate_access_token,
};

pub use eventsub::{
    ChatBadge, ChatMessage, ChatMessageDeleted, ChatSubscriptionConfig, EventSubAuthError,
    EventSubChatStream, EventSubStreamEvent, connect_chat_stream,
};

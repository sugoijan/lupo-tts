use anyhow::{Context, Result};
use messages::{
    EmoteId, EmoteImage, EmoteImageFormat, EmoteImageScale, EmoteProvider, EmoteRef, EmoteResolver,
    EmoteThemeMode, ResolvedSegment, resolve_with_lookup,
};
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

const API_BASE: &str = "https://api.betterttv.net";
const CDN_BASE: &str = "https://cdn.betterttv.net/emote";

#[derive(Clone)]
pub struct BttvClient {
    http: Client,
}

impl BttvClient {
    pub fn new() -> Result<Self> {
        let http = Client::builder()
            .user_agent("lupo-tts/0.0.1")
            .build()
            .context("failed to construct BetterTTV HTTP client")?;
        Ok(Self { http })
    }

    pub async fn fetch_global_emotes(&self) -> Result<BttvEmoteSet> {
        let emotes = self
            .http
            .get(format!("{API_BASE}/3/cached/emotes/global"))
            .send()
            .await
            .context("failed to query BetterTTV global emotes")?
            .error_for_status()
            .context("BetterTTV global emote request failed")?
            .json::<Vec<EmoteModel>>()
            .await
            .context("failed to parse BetterTTV global emote response")?;

        Ok(BttvEmoteSet::from_models(
            "global",
            "BetterTTV Global",
            emotes,
        ))
    }

    pub async fn fetch_channel_emotes_by_twitch_id(
        &self,
        twitch_id: &str,
    ) -> Result<BttvChannelEmoteSets> {
        let response = self
            .http
            .get(format!("{API_BASE}/3/cached/users/twitch/{twitch_id}"))
            .send()
            .await
            .context("failed to query BetterTTV channel emotes")?
            .error_for_status()
            .context("BetterTTV channel emote request failed")?
            .json::<ChannelResponse>()
            .await
            .context("failed to parse BetterTTV channel emote response")?;

        Ok(BttvChannelEmoteSets::from_response(twitch_id, response))
    }
}

#[derive(Debug, Clone)]
pub struct BttvEmoteSet {
    pub id: String,
    pub name: String,
    pub emotes: Vec<Arc<EmoteRef>>,
}

impl BttvEmoteSet {
    fn from_models(
        id: impl Into<String>,
        name: impl Into<String>,
        models: Vec<EmoteModel>,
    ) -> Self {
        let emotes = models
            .into_iter()
            .map(|model| Arc::new(emote_from_model(model)))
            .collect();
        Self {
            id: id.into(),
            name: name.into(),
            emotes,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.emotes.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct BttvChannelEmoteSets {
    pub sets: Vec<BttvEmoteSet>,
}

impl BttvChannelEmoteSets {
    fn from_response(twitch_id: &str, response: ChannelResponse) -> Self {
        let mut sets = Vec::new();

        if !response.channel_emotes.is_empty() {
            sets.push(BttvEmoteSet::from_models(
                format!("channel:{twitch_id}"),
                "Channel Emotes",
                response.channel_emotes,
            ));
        }

        if !response.shared_emotes.is_empty() {
            sets.push(BttvEmoteSet::from_models(
                format!("shared:{twitch_id}"),
                "Shared Emotes",
                response.shared_emotes,
            ));
        }

        Self { sets }
    }

    pub fn is_empty(&self) -> bool {
        self.sets.iter().all(|set| set.emotes.is_empty())
    }

    pub fn all_emotes(&self) -> Vec<Arc<EmoteRef>> {
        self.sets
            .iter()
            .flat_map(|set| set.emotes.iter().cloned())
            .collect()
    }
}

#[derive(Default)]
pub struct BttvCatalog {
    global: HashMap<String, Arc<EmoteRef>>,
    channels: HashMap<String, HashMap<String, Arc<EmoteRef>>>,
}

impl BttvCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_global_emotes<I>(&mut self, emotes: I)
    where
        I: IntoIterator<Item = Arc<EmoteRef>>,
    {
        for emote in emotes {
            self.global.insert(emote.code.clone(), emote);
        }
    }

    pub fn insert_channel_emotes<S, I>(&mut self, channel_id: S, emotes: I)
    where
        S: Into<String>,
        I: IntoIterator<Item = Arc<EmoteRef>>,
    {
        let channel_id = channel_id.into();
        let mut map = HashMap::new();
        for emote in emotes {
            map.insert(emote.code.clone(), emote);
        }
        if map.is_empty() {
            return;
        }
        self.channels.insert(channel_id, map);
    }

    pub fn is_empty(&self) -> bool {
        self.global.is_empty() && self.channels.is_empty()
    }

    pub fn global_resolver(&self) -> Option<Arc<dyn EmoteResolver>> {
        if self.global.is_empty() {
            return None;
        }
        Some(Arc::new(BttvGlobalResolver::new(self.global.clone())) as Arc<dyn EmoteResolver>)
    }

    pub fn channel_resolver(&self, channel_id: &str) -> Option<Arc<dyn EmoteResolver>> {
        self.channels.get(channel_id).cloned().map(|emotes| {
            Arc::new(BttvChannelResolver::new(channel_id, emotes)) as Arc<dyn EmoteResolver>
        })
    }
}

struct BttvGlobalResolver {
    emotes: Arc<HashMap<String, Arc<EmoteRef>>>,
}

impl BttvGlobalResolver {
    fn new(emotes: HashMap<String, Arc<EmoteRef>>) -> Self {
        Self {
            emotes: Arc::new(emotes),
        }
    }
}

impl EmoteResolver for BttvGlobalResolver {
    fn resolve(&self, text_segment: &str) -> Vec<ResolvedSegment> {
        resolve_with_lookup(text_segment, |token| self.emotes.get(token).cloned())
    }
}

struct BttvChannelResolver {
    _channel_id: Arc<str>,
    emotes: Arc<HashMap<String, Arc<EmoteRef>>>,
}

impl BttvChannelResolver {
    fn new(channel_id: &str, emotes: HashMap<String, Arc<EmoteRef>>) -> Self {
        Self {
            _channel_id: Arc::from(channel_id.to_string()),
            emotes: Arc::new(emotes),
        }
    }
}

impl EmoteResolver for BttvChannelResolver {
    fn resolve(&self, text_segment: &str) -> Vec<ResolvedSegment> {
        resolve_with_lookup(text_segment, |token| self.emotes.get(token).cloned())
    }
}

#[derive(Deserialize)]
struct EmoteModel {
    id: String,
    code: String,
    #[serde(rename = "imageType", default)]
    image_type: String,
    #[serde(default)]
    animated: bool,
}

#[derive(Deserialize)]
struct ChannelResponse {
    #[serde(rename = "channelEmotes", default)]
    channel_emotes: Vec<EmoteModel>,
    #[serde(rename = "sharedEmotes", default)]
    shared_emotes: Vec<EmoteModel>,
}

fn emote_from_model(model: EmoteModel) -> EmoteRef {
    let is_animated = model.animated
        || matches!(
            model.image_type.to_ascii_lowercase().as_str(),
            "gif" | "webm"
        );
    let images = build_image_variants(&model.id, is_animated);

    EmoteRef::new(
        EmoteId::new(EmoteProvider::BetterTtv, model.id),
        model.code,
        is_animated,
        images,
    )
}

fn build_image_variants(emote_id: &str, is_animated: bool) -> Vec<EmoteImage> {
    let mut images = Vec::new();
    let format = if is_animated {
        EmoteImageFormat::Animated
    } else {
        EmoteImageFormat::Static
    };

    images.push(EmoteImage {
        format: format.clone(),
        theme_mode: EmoteThemeMode::Light,
        scale: EmoteImageScale::One,
        url: format!("{CDN_BASE}/{emote_id}/1x"),
    });
    images.push(EmoteImage {
        format: format.clone(),
        theme_mode: EmoteThemeMode::Light,
        scale: EmoteImageScale::Two,
        url: format!("{CDN_BASE}/{emote_id}/2x"),
    });
    images.push(EmoteImage {
        format,
        theme_mode: EmoteThemeMode::Light,
        scale: EmoteImageScale::Three,
        url: format!("{CDN_BASE}/{emote_id}/3x"),
    });

    images
}

#[cfg(test)]
mod tests {
    use super::*;
    use messages::ResolvedSegmentKind;

    #[test]
    fn builds_emote_refs_from_models() {
        let model = EmoteModel {
            id: "1234".to_string(),
            code: "CoolCat".to_string(),
            image_type: "png".to_string(),
            animated: false,
        };

        let emote = emote_from_model(model);
        assert_eq!(emote.code, "CoolCat");
        assert_eq!(emote.id.value, "1234");
        assert!(!emote.is_animated);
        assert_eq!(emote.images.len(), 3);
        assert!(
            emote
                .images
                .iter()
                .all(|image| matches!(image.format, EmoteImageFormat::Static))
        );
    }

    #[test]
    fn catalog_resolves_channel_and_global_emotes() {
        let mut catalog = BttvCatalog::new();

        let global_emote = Arc::new(EmoteRef::new(
            EmoteId::new(EmoteProvider::BetterTtv, "global"),
            "GlobalSmile",
            false,
            Vec::new(),
        ));
        catalog.insert_global_emotes(vec![global_emote.clone()]);

        let channel_emote = Arc::new(EmoteRef::new(
            EmoteId::new(EmoteProvider::BetterTtv, "channel"),
            "ChannelSmile",
            false,
            Vec::new(),
        ));
        catalog.insert_channel_emotes("123", vec![channel_emote.clone()]);

        let channel_resolver = catalog
            .channel_resolver("123")
            .expect("expected channel resolver");
        let channel_segments = channel_resolver.resolve("ChannelSmile");
        assert!(channel_segments.len() == 1);
        match &channel_segments[0].kind {
            ResolvedSegmentKind::Emote { emote, .. } => {
                assert_eq!(emote.code, "ChannelSmile");
            }
            other => panic!("expected emote segment, got {other:?}"),
        }

        assert!(catalog.channel_resolver("999").is_none());

        let global_resolver = catalog.global_resolver().expect("expected global resolver");
        let global_segments = global_resolver.resolve("GlobalSmile");
        assert!(global_segments.len() == 1);
        match &global_segments[0].kind {
            ResolvedSegmentKind::Emote { emote, .. } => {
                assert_eq!(emote.code, "GlobalSmile");
            }
            other => panic!("expected emote segment, got {other:?}"),
        }
    }
}

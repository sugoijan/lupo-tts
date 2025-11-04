use anyhow::{Context, Result, anyhow};
use messages::{
    EmoteId, EmoteImage, EmoteImageFormat, EmoteImageScale, EmoteModifier, EmoteModifierFlags,
    EmoteProvider, EmoteRef, EmoteResolver, EmoteThemeMode, ResolvedSegment, resolve_with_lookup,
};
use reqwest::Client;
use serde::Deserialize;
use std::borrow::Borrow;
use std::collections::HashMap;
use std::sync::Arc;

const API_BASE: &str = "https://api.frankerfacez.com/v1";

#[derive(Clone)]
pub struct FfzClient {
    http: Client,
}

impl FfzClient {
    pub fn new() -> Result<Self> {
        let http = Client::builder()
            .user_agent("lupo-tts/0.0.1")
            .build()
            .context("failed to construct FFZ HTTP client")?;
        Ok(Self { http })
    }

    pub async fn fetch_global_emotes(&self) -> Result<FfzGlobalEmoteSets> {
        let response = self
            .http
            .get(format!("{API_BASE}/set/global"))
            .send()
            .await
            .context("failed to query FFZ global emotes")?
            .error_for_status()
            .context("FFZ global emote request failed")?
            .json::<GlobalSetsResponse>()
            .await
            .context("failed to parse FFZ global emotes response")?;

        Ok(FfzGlobalEmoteSets::from_response(response))
    }

    pub async fn fetch_room_emotes_by_twitch_id(
        &self,
        twitch_id: &str,
    ) -> Result<FfzRoomEmoteSets> {
        let response = self
            .http
            .get(format!("{API_BASE}/room/id/{twitch_id}"))
            .send()
            .await
            .context("failed to query FFZ room by Twitch ID")?
            .error_for_status()
            .context("FFZ room lookup failed")?
            .json::<RoomResponse>()
            .await
            .context("failed to parse FFZ room response")?;

        FfzRoomEmoteSets::from_response(response)
    }
}

#[derive(Debug, Clone)]
pub struct FfzEmoteSet {
    pub id: String,
    pub title: Option<String>,
    pub emotes: Vec<Arc<EmoteRef>>,
}

impl FfzEmoteSet {
    fn from_model(model: &EmoteSetModel) -> Self {
        let emotes = model
            .emoticons
            .iter()
            .map(|emote| Arc::new(emote_from_model(emote)))
            .collect();

        Self {
            id: model.id.to_string(),
            title: model.title.clone(),
            emotes,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.emotes.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct FfzGlobalEmoteSets {
    sets: HashMap<String, Arc<FfzEmoteSet>>,
    default_set_ids: Vec<String>,
    user_scoped: HashMap<String, Vec<String>>,
}

impl FfzGlobalEmoteSets {
    fn from_response(response: GlobalSetsResponse) -> Self {
        let mut sets = HashMap::new();
        for (set_id, model) in response.sets {
            sets.insert(set_id, Arc::new(FfzEmoteSet::from_model(&model)));
        }

        let default_set_ids = response
            .default_sets
            .into_iter()
            .map(|id| id.to_string())
            .collect();

        let user_scoped = response.users;

        Self {
            sets,
            default_set_ids,
            user_scoped,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.default_set_ids.is_empty() && self.user_scoped.is_empty()
    }

    pub fn default_sets(&self) -> Vec<Arc<FfzEmoteSet>> {
        self.default_set_ids
            .iter()
            .filter_map(|id| self.sets.get(id).cloned())
            .collect()
    }

    pub fn sets_for_user(&self, user_id: &str) -> Vec<Arc<FfzEmoteSet>> {
        self.user_scoped
            .iter()
            .filter_map(|(set_id, users)| {
                if users.iter().any(|candidate| candidate == user_id) {
                    self.sets.get(set_id).cloned()
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn user_scoped_summary(&self) -> Vec<(Arc<FfzEmoteSet>, usize)> {
        self.user_scoped
            .iter()
            .filter_map(|(set_id, users)| {
                self.sets.get(set_id).cloned().map(|set| (set, users.len()))
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct FfzRoomEmoteSets {
    pub room_id: String,
    pub twitch_id: String,
    pub sets: Vec<FfzEmoteSet>,
}

impl FfzRoomEmoteSets {
    fn from_response(response: RoomResponse) -> Result<Self> {
        let room = response
            .room
            .ok_or_else(|| anyhow!("FFZ room response missing room payload"))?;

        let mut sets = Vec::new();
        for model in response.sets.values() {
            sets.push(FfzEmoteSet::from_model(model));
        }

        Ok(Self {
            room_id: room.id.clone(),
            twitch_id: room.twitch_id.to_string(),
            sets,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }
}

#[derive(Default)]
pub struct FfzCatalog {
    global: HashMap<String, Arc<EmoteRef>>,
    channels: HashMap<String, HashMap<String, Arc<EmoteRef>>>,
}

impl FfzCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_global_sets<I>(&mut self, sets: I)
    where
        I: IntoIterator,
        I::Item: Borrow<FfzEmoteSet>,
    {
        for set in sets {
            let set = set.borrow();
            for emote in &set.emotes {
                self.global.insert(emote.code.clone(), Arc::clone(emote));
            }
        }
    }

    pub fn insert_channel_sets<S, I>(&mut self, channel_id: S, sets: I)
    where
        S: Into<String>,
        I: IntoIterator,
        I::Item: Borrow<FfzEmoteSet>,
    {
        let entry = self.channels.entry(channel_id.into()).or_default();
        for set in sets {
            let set = set.borrow();
            for emote in &set.emotes {
                entry.insert(emote.code.clone(), Arc::clone(emote));
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.global.is_empty() && self.channels.is_empty()
    }

    pub fn global_resolver(&self) -> Option<Arc<dyn EmoteResolver>> {
        if self.global.is_empty() {
            return None;
        }
        Some(Arc::new(FfzGlobalResolver::new(self.global.clone())) as Arc<dyn EmoteResolver>)
    }

    pub fn channel_resolver(&self, channel_id: &str) -> Option<Arc<dyn EmoteResolver>> {
        self.channels.get(channel_id).cloned().map(|emotes| {
            Arc::new(FfzChannelResolver::new(channel_id, emotes)) as Arc<dyn EmoteResolver>
        })
    }
}

struct FfzGlobalResolver {
    emotes: Arc<HashMap<String, Arc<EmoteRef>>>,
}

impl FfzGlobalResolver {
    fn new(emotes: HashMap<String, Arc<EmoteRef>>) -> Self {
        Self {
            emotes: Arc::new(emotes),
        }
    }
}

impl EmoteResolver for FfzGlobalResolver {
    fn resolve(&self, text_segment: &str) -> Vec<ResolvedSegment> {
        resolve_with_lookup(text_segment, |token| self.emotes.get(token).cloned())
    }
}

struct FfzChannelResolver {
    _channel_id: Arc<str>,
    emotes: Arc<HashMap<String, Arc<EmoteRef>>>,
}

impl FfzChannelResolver {
    fn new(channel_id: &str, emotes: HashMap<String, Arc<EmoteRef>>) -> Self {
        Self {
            _channel_id: Arc::from(channel_id.to_string()),
            emotes: Arc::new(emotes),
        }
    }
}

impl EmoteResolver for FfzChannelResolver {
    fn resolve(&self, text_segment: &str) -> Vec<ResolvedSegment> {
        resolve_with_lookup(text_segment, |token| self.emotes.get(token).cloned())
    }
}

#[derive(Deserialize)]
struct GlobalSetsResponse {
    #[serde(default)]
    default_sets: Vec<u64>,
    #[serde(default)]
    sets: HashMap<String, EmoteSetModel>,
    #[serde(default)]
    users: HashMap<String, Vec<String>>,
}

#[derive(Deserialize)]
struct RoomResponse {
    room: Option<RoomModel>,
    #[serde(default)]
    sets: HashMap<String, EmoteSetModel>,
}

#[derive(Deserialize)]
struct RoomModel {
    #[serde(rename = "id")]
    id: String,
    #[serde(rename = "twitch_id")]
    twitch_id: u64,
}

#[derive(Deserialize)]
struct EmoteSetModel {
    id: u64,
    title: Option<String>,
    #[serde(default)]
    emoticons: Vec<EmoteModel>,
}

type ImageMap = HashMap<String, Option<String>>;

#[derive(Deserialize)]
struct EmoteModel {
    id: u64,
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    public: bool,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    modifier: bool,
    #[serde(default)]
    modifier_flags: u32,
    #[serde(default)]
    urls: ImageMap,
    animated: Option<ImageMap>,
    mask: Option<ImageMap>,
    mask_animated: Option<ImageMap>,
}

fn emote_from_model(model: &EmoteModel) -> EmoteRef {
    let mut images = Vec::new();
    images.extend(image_variants(&model.urls, EmoteImageFormat::Static));
    if let Some(animated) = &model.animated {
        images.extend(image_variants(animated, EmoteImageFormat::Animated));
    }

    let is_animated = images
        .iter()
        .any(|image| matches!(image.format, EmoteImageFormat::Animated));

    let mut emote = EmoteRef::new(
        EmoteId::new(EmoteProvider::FrankerFaceZ, model.id.to_string()),
        model.name.clone(),
        is_animated,
        images,
    );

    if model.modifier {
        let raw_flags = model.modifier_flags;
        let flags = EmoteModifierFlags::from_bits_retain(raw_flags);
        let mut mask_images = Vec::new();
        if let Some(mask) = &model.mask {
            mask_images.extend(image_variants(mask, EmoteImageFormat::Static));
        }
        if let Some(mask) = &model.mask_animated {
            mask_images.extend(image_variants(mask, EmoteImageFormat::Animated));
        }
        let modifier = EmoteModifier {
            flags,
            raw_flags,
            is_hidden: model.hidden || flags.contains(EmoteModifierFlags::HIDDEN),
            mask_images,
        };
        emote = emote.with_modifier(modifier);
    }

    emote
}

fn image_variants(images: &ImageMap, format: EmoteImageFormat) -> Vec<EmoteImage> {
    images
        .iter()
        .filter_map(|(scale_key, url)| {
            let url = url.clone()?;
            let scale = parse_scale(scale_key);
            Some(EmoteImage {
                format: format.clone(),
                theme_mode: EmoteThemeMode::Light,
                scale,
                url,
            })
        })
        .collect()
}

fn parse_scale(value: &str) -> EmoteImageScale {
    match value {
        "1" | "1.0" => EmoteImageScale::One,
        "2" | "2.0" => EmoteImageScale::Two,
        "3" | "3.0" => EmoteImageScale::Three,
        other => EmoteImageScale::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_modifier_metadata_from_emote_model() {
        let mut urls = ImageMap::new();
        urls.insert(
            "1".to_string(),
            Some("https://cdn.example.com/emote.png".to_string()),
        );
        let mut animated = ImageMap::new();
        animated.insert(
            "1".to_string(),
            Some("https://cdn.example.com/emote.webp".to_string()),
        );
        let mut mask = ImageMap::new();
        mask.insert(
            "1".to_string(),
            Some("https://cdn.example.com/mask.png".to_string()),
        );

        let model = EmoteModel {
            id: 42,
            name: "RainbowMod".to_string(),
            public: true,
            hidden: false,
            modifier: true,
            modifier_flags: (EmoteModifierFlags::HIDDEN | EmoteModifierFlags::RAINBOW).bits(),
            urls,
            animated: Some(animated),
            mask: Some(mask),
            mask_animated: None,
        };

        let emote = emote_from_model(&model);
        assert!(emote.is_modifier());
        assert!(emote.is_animated);
        let modifier = emote.modifier().expect("modifier metadata missing");
        assert!(modifier.is_hidden);
        assert!(modifier.flags.contains(EmoteModifierFlags::RAINBOW));
        assert_eq!(modifier.mask_images.len(), 1);
    }
}

use anyhow::{Context, Result, anyhow};
use messages::{
    EmoteId, EmoteImage, EmoteImageFormat, EmoteImageScale, EmoteProvider, EmoteRef, EmoteResolver,
    EmoteThemeMode, MapEmoteResolver,
};
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

const API_BASE: &str = "https://7tv.io/v3";

#[derive(Clone)]
pub struct SevenTvClient {
    http: Client,
}

impl SevenTvClient {
    pub fn new() -> Result<Self> {
        let http = Client::builder()
            .user_agent("lupo-tts/0.0.1")
            .build()
            .context("failed to construct 7tv HTTP client")?;
        Ok(Self { http })
    }

    pub async fn fetch_global_emotes(&self) -> Result<SevenTvEmoteSet> {
        self.fetch_emote_set("global").await
    }

    pub async fn fetch_channel_emotes_by_twitch_id(
        &self,
        twitch_id: &str,
    ) -> Result<SevenTvEmoteSet> {
        let user = self
            .http
            .get(format!("{API_BASE}/users/twitch/{twitch_id}"))
            .send()
            .await
            .context("failed to query 7tv user by twitch id")?
            .error_for_status()
            .context("7tv user lookup failed")?
            .json::<SevenTvUser>()
            .await
            .context("failed to parse 7tv user response")?;

        let default_set_id = user
            .default_emote_set_id()
            .ok_or_else(|| anyhow!("7tv user had no associated twitch emote set"))?;

        self.fetch_emote_set(&default_set_id).await
    }

    async fn fetch_emote_set(&self, set_id: &str) -> Result<SevenTvEmoteSet> {
        let response = self
            .http
            .get(format!("{API_BASE}/emote-sets/{set_id}"))
            .send()
            .await
            .context("failed to query 7tv emote set")?
            .error_for_status()
            .context("7tv emote set request failed")?
            .json::<EmoteSetModel>()
            .await
            .context("failed to parse 7tv emote set response")?;

        Ok(SevenTvEmoteSet::from_model(response))
    }
}

#[derive(Debug, Clone)]
pub struct SevenTvEmoteSet {
    pub id: String,
    pub name: String,
    pub emotes: Vec<Arc<EmoteRef>>,
}

impl SevenTvEmoteSet {
    fn from_model(model: EmoteSetModel) -> Self {
        let emotes = model
            .emotes
            .into_iter()
            .map(|entry| Arc::new(emote_from_model(entry)))
            .collect();
        Self {
            id: model.id,
            name: model.name,
            emotes,
        }
    }
}

#[derive(Default)]
pub struct SevenTvCatalog {
    global: HashMap<String, Arc<EmoteRef>>,
    channels: HashMap<String, HashMap<String, Arc<EmoteRef>>>,
}

impl SevenTvCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_global_set(&mut self, set: &SevenTvEmoteSet) {
        for emote in &set.emotes {
            self.global.insert(emote.code.clone(), Arc::clone(emote));
        }
    }

    pub fn insert_channel_set<S: Into<String>>(&mut self, channel_id: S, set: &SevenTvEmoteSet) {
        let channel_id = channel_id.into();
        let mut map = HashMap::new();
        for emote in &set.emotes {
            map.insert(emote.code.clone(), Arc::clone(emote));
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
        Some(Arc::new(MapEmoteResolver::new(self.global.clone())) as Arc<dyn EmoteResolver>)
    }

    pub fn channel_resolver(&self, channel_id: &str) -> Option<Arc<dyn EmoteResolver>> {
        self.channels
            .get(channel_id)
            .cloned()
            .map(|emotes| Arc::new(MapEmoteResolver::new(emotes)) as Arc<dyn EmoteResolver>)
    }
}

#[derive(Deserialize)]
struct EmoteSetModel {
    id: String,
    name: String,
    emotes: Vec<SetEmote>,
}

#[derive(Deserialize)]
struct SetEmote {
    id: String,
    name: String,
    data: EmoteData,
}

#[derive(Deserialize)]
struct EmoteData {
    host: EmoteHost,
    #[serde(default)]
    animated: bool,
}

#[derive(Deserialize)]
struct EmoteHost {
    url: String,
    #[serde(default)]
    files: Vec<HostFile>,
}

#[derive(Deserialize)]
struct HostFile {
    name: String,
    #[serde(default)]
    format: String,
}

#[derive(Deserialize)]
struct SevenTvUser {
    #[serde(default)]
    connections: Vec<UserConnection>,
    #[serde(default)]
    emote_set: Option<UserSetRef>,
    #[serde(default)]
    emote_sets: Vec<UserSetRef>,
}

#[derive(Deserialize)]
struct UserConnection {
    platform: String,
    #[serde(default)]
    emote_set: Option<UserSetRef>,
    #[serde(default)]
    emote_set_id: Option<String>,
}

#[derive(Deserialize)]
struct UserSetRef {
    id: String,
}

impl SevenTvUser {
    fn default_emote_set_id(&self) -> Option<String> {
        if let Some(set) = &self.emote_set {
            return Some(set.id.clone());
        }
        for conn in &self.connections {
            if conn.platform.eq_ignore_ascii_case("twitch") {
                if let Some(set) = &conn.emote_set {
                    return Some(set.id.clone());
                }
                if let Some(id) = &conn.emote_set_id {
                    return Some(id.clone());
                }
            }
        }
        self.emote_sets.first().map(|set| set.id.clone())
    }
}

fn emote_from_model(model: SetEmote) -> EmoteRef {
    let images = host_files_to_images(&model.data.host);
    EmoteRef::new(
        EmoteId::new(EmoteProvider::SevenTv, model.id),
        model.name,
        model.data.animated,
        images,
    )
}

fn host_files_to_images(host: &EmoteHost) -> Vec<EmoteImage> {
    let mut images = Vec::new();
    let base = host.url.trim_end_matches('/');
    for file in &host.files {
        let url = if base.is_empty() {
            file.name.clone()
        } else {
            format!("{}/{}", base, file.name.trim_start_matches('/'))
        };
        let format = match file.format.to_ascii_lowercase().as_str() {
            "gif" | "webm" => EmoteImageFormat::Animated,
            _ => EmoteImageFormat::Static,
        };
        let scale = match file.name.split('.').next().unwrap_or("") {
            "1x" => EmoteImageScale::One,
            "2x" => EmoteImageScale::Two,
            "3x" => EmoteImageScale::Three,
            other => EmoteImageScale::Other(other.to_string()),
        };
        images.push(EmoteImage {
            format,
            theme_mode: EmoteThemeMode::Light,
            scale,
            url,
        });
    }
    images
}

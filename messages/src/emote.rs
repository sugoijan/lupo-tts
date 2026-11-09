use std::{collections::HashMap, ops::Range, sync::Arc};

use bitflags::bitflags;
use emojis;
use once_cell::sync::Lazy;
use tracing::warn;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EmoteProvider {
    Twitch,
    BetterTtv,
    SevenTv,
    FrankerFaceZ,
    Emoji,
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EmoteId {
    pub provider: EmoteProvider,
    pub value: String,
}

impl EmoteId {
    pub fn new(provider: EmoteProvider, value: impl Into<String>) -> Self {
        Self {
            provider,
            value: value.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EmoteImageFormat {
    Static,
    Animated,
    Other(String),
}

impl EmoteImageFormat {
    pub fn parse(value: &str) -> Self {
        match value {
            "static" => Self::Static,
            "animated" => Self::Animated,
            other => Self::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            EmoteImageFormat::Static => "static",
            EmoteImageFormat::Animated => "animated",
            EmoteImageFormat::Other(value) => value.as_str(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EmoteThemeMode {
    Light,
    Dark,
    Other(String),
}

impl EmoteThemeMode {
    pub fn parse(value: &str) -> Self {
        match value {
            "light" => Self::Light,
            "dark" => Self::Dark,
            other => Self::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            EmoteThemeMode::Light => "light",
            EmoteThemeMode::Dark => "dark",
            EmoteThemeMode::Other(value) => value.as_str(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EmoteImageScale {
    One,
    Two,
    Three,
    Other(String),
}

impl EmoteImageScale {
    pub fn parse(value: &str) -> Self {
        match value {
            "1.0" => Self::One,
            "2.0" => Self::Two,
            "3.0" => Self::Three,
            other => Self::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            EmoteImageScale::One => "1.0",
            EmoteImageScale::Two => "2.0",
            EmoteImageScale::Three => "3.0",
            EmoteImageScale::Other(value) => value.as_str(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct EmoteImage {
    pub format: EmoteImageFormat,
    pub theme_mode: EmoteThemeMode,
    pub scale: EmoteImageScale,
    pub url: String,
}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct EmoteModifierFlags: u32 {
        const HIDDEN = 1 << 0;
        const FLIP_X = 1 << 1;
        const FLIP_Y = 1 << 2;
        const GROW_X = 1 << 3;
        const RAINBOW = 1 << 11;
        const HYPER_RED = 1 << 12;
        const HYPER_SHAKE = 1 << 13;
        const CURSED = 1 << 14;
        const JAM = 1 << 15;
        const BOUNCE = 1 << 16;
    }
}

#[derive(Clone, Debug)]
pub struct EmoteModifier {
    pub flags: EmoteModifierFlags,
    pub raw_flags: u32,
    pub is_hidden: bool,
    pub mask_images: Vec<EmoteImage>,
}

impl Default for EmoteModifier {
    fn default() -> Self {
        Self {
            flags: EmoteModifierFlags::empty(),
            raw_flags: 0,
            is_hidden: false,
            mask_images: Vec::new(),
        }
    }
}

impl EmoteModifier {
    pub fn has_flag(&self, flag: EmoteModifierFlags) -> bool {
        self.flags.contains(flag)
    }
}

#[derive(Clone, Debug)]
pub struct EmoteRef {
    pub id: EmoteId,
    pub code: String,
    pub is_animated: bool,
    pub images: Vec<EmoteImage>,
    pub modifier: Option<EmoteModifier>,
}

impl EmoteRef {
    pub fn new(
        id: EmoteId,
        code: impl Into<String>,
        is_animated: bool,
        images: Vec<EmoteImage>,
    ) -> Self {
        Self {
            id,
            code: code.into(),
            is_animated,
            images,
            modifier: None,
        }
    }

    pub fn with_modifier(mut self, modifier: EmoteModifier) -> Self {
        self.modifier = Some(modifier);
        self
    }

    pub fn is_modifier(&self) -> bool {
        self.modifier.is_some()
    }

    pub fn modifier(&self) -> Option<&EmoteModifier> {
        self.modifier.as_ref()
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedModifier {
    pub range: Range<usize>,
    pub emote: Arc<EmoteRef>,
}

impl ResolvedModifier {
    pub fn new(range: Range<usize>, emote: Arc<EmoteRef>) -> Self {
        Self { range, emote }
    }

    fn shift(&mut self, offset: usize) {
        self.range.start += offset;
        self.range.end += offset;
    }
}

#[derive(Clone, Debug)]
pub enum ResolvedSegmentKind {
    Text,
    Emote {
        emote: Arc<EmoteRef>,
        modifiers: Vec<ResolvedModifier>,
    },
    Unknown,
}

#[derive(Clone, Debug)]
pub struct ResolvedSegment {
    pub range: Range<usize>,
    pub kind: ResolvedSegmentKind,
}

impl ResolvedSegment {
    pub fn text(range: Range<usize>) -> Self {
        Self {
            range,
            kind: ResolvedSegmentKind::Text,
        }
    }

    pub fn text_full(text_segment: &str) -> Self {
        Self::text(0..text_segment.len())
    }

    pub fn emote(range: Range<usize>, emote: Arc<EmoteRef>) -> Self {
        Self {
            range,
            kind: ResolvedSegmentKind::Emote {
                emote,
                modifiers: Vec::new(),
            },
        }
    }

    pub fn unknown(range: Range<usize>) -> Self {
        Self {
            range,
            kind: ResolvedSegmentKind::Unknown,
        }
    }

    pub fn range(&self) -> &Range<usize> {
        &self.range
    }

    pub fn is_text(&self) -> bool {
        matches!(self.kind, ResolvedSegmentKind::Text)
    }

    fn shift(&mut self, offset: usize) {
        self.range.start += offset;
        self.range.end += offset;
        if let ResolvedSegmentKind::Emote { modifiers, .. } = &mut self.kind {
            for modifier in modifiers {
                modifier.shift(offset);
            }
        }
    }
}

pub trait EmoteResolver: Send + Sync {
    fn resolve(&self, text_segment: &str) -> Vec<ResolvedSegment>;
}

/// Resolves emote codes from a fixed catalog snapshot.
/// Channel selection and provider precedence belong to [`ResolverRegistry`].
pub struct MapEmoteResolver {
    emotes: HashMap<String, Arc<EmoteRef>>,
}

impl MapEmoteResolver {
    pub fn new(emotes: HashMap<String, Arc<EmoteRef>>) -> Self {
        Self { emotes }
    }
}

impl EmoteResolver for MapEmoteResolver {
    fn resolve(&self, text_segment: &str) -> Vec<ResolvedSegment> {
        resolve_with_lookup(text_segment, |token| self.emotes.get(token).cloned())
    }
}

#[derive(Default)]
pub struct ResolverRegistry {
    global: Vec<Arc<dyn EmoteResolver>>,
    channels: HashMap<String, Vec<Arc<dyn EmoteResolver>>>,
}

impl ResolverRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_global(&mut self, resolver: Arc<dyn EmoteResolver>) {
        self.global.push(resolver);
    }

    pub fn add_channel<S>(&mut self, channel_id: S, resolver: Arc<dyn EmoteResolver>)
    where
        S: Into<String>,
    {
        self.channels
            .entry(channel_id.into())
            .or_default()
            .push(resolver);
    }

    pub fn clear_channel(&mut self, channel_id: &str) {
        self.channels.remove(channel_id);
    }

    pub fn is_empty(&self) -> bool {
        self.global.is_empty() && self.channels.is_empty()
    }

    pub fn resolve(&self, channel_id: Option<&str>, text_segment: &str) -> Vec<ResolvedSegment> {
        let mut segments = vec![ResolvedSegment::text_full(text_segment)];
        if let Some(channel_id) = channel_id
            && let Some(resolvers) = self.channels.get(channel_id)
        {
            segments = apply_resolver_chain(resolvers, text_segment, segments);
        }
        apply_resolver_chain(&self.global, text_segment, segments)
    }
}

fn apply_resolver_chain(
    resolvers: &[Arc<dyn EmoteResolver>],
    text_segment: &str,
    mut segments: Vec<ResolvedSegment>,
) -> Vec<ResolvedSegment> {
    for resolver in resolvers {
        let mut next = Vec::with_capacity(segments.len());
        for segment in segments {
            if segment.is_text() {
                let slice = &text_segment[segment.range.clone()];
                let mut resolved = resolver.resolve(slice);
                if !validate_resolved_segments(slice, &resolved) {
                    warn!("emote resolver returned invalid segmentation; dropping result");
                    next.push(segment);
                    continue;
                }
                for mut entry in resolved.drain(..) {
                    entry.shift(segment.range.start);
                    next.push(entry);
                }
            } else {
                next.push(segment);
            }
        }
        segments = next;
    }
    segments
}

pub fn validate_resolved_segments(text_segment: &str, segments: &[ResolvedSegment]) -> bool {
    let len = text_segment.len();
    if len == 0 {
        return segments.iter().all(|segment| {
            segment.range.start == 0 && segment.range.end == 0 && segment.is_text()
        });
    }

    if segments.is_empty() {
        return false;
    }

    let mut previous_end = 0;
    for segment in segments {
        if segment.range.start < previous_end || segment.range.end > len {
            return false;
        }
        if segment.range.start > segment.range.end {
            return false;
        }
        if segment.range.start == segment.range.end {
            return false;
        }
        if let ResolvedSegmentKind::Emote { modifiers, .. } = &segment.kind {
            for modifier in modifiers {
                if modifier.range.start > modifier.range.end {
                    return false;
                }
                if modifier.range.start == modifier.range.end {
                    return false;
                }
                if modifier.range.end > len {
                    return false;
                }
            }
        }
        previous_end = segment.range.end;
    }

    let mut coverage: Vec<Range<usize>> = segments
        .iter()
        .map(|segment| segment.range.clone())
        .collect();
    for segment in segments {
        if let ResolvedSegmentKind::Emote { modifiers, .. } = &segment.kind {
            for modifier in modifiers {
                coverage.push(modifier.range.clone());
            }
        }
    }
    coverage.sort_by_key(|range| range.start);

    let mut cursor = 0;
    for range in coverage {
        if range.start != cursor {
            return false;
        }
        if range.end < range.start || range.end > len {
            return false;
        }
        cursor = range.end;
    }

    cursor == len
}

pub fn resolve_with_lookup<F>(text_segment: &str, lookup: F) -> Vec<ResolvedSegment>
where
    F: Fn(&str) -> Option<Arc<EmoteRef>>,
{
    if text_segment.is_empty() {
        return Vec::new();
    }

    let mut segments = Vec::new();
    let mut token_start: Option<usize> = None;
    let mut last_emote_idx: Option<usize> = None;

    for (offset, grapheme) in text_segment.grapheme_indices(true) {
        let start = offset;
        let end = start + grapheme.len();

        if grapheme.chars().all(|ch| ch.is_whitespace()) {
            if let Some(begin) = token_start.take() {
                push_token(
                    &lookup,
                    text_segment,
                    begin,
                    start,
                    &mut segments,
                    &mut last_emote_idx,
                );
            }
            segments.push(ResolvedSegment::text(start..end));
            continue;
        }

        if token_start.is_none() {
            token_start = Some(start);
        }
    }

    if let Some(begin) = token_start {
        push_token(
            &lookup,
            text_segment,
            begin,
            text_segment.len(),
            &mut segments,
            &mut last_emote_idx,
        );
    }

    if segments.is_empty() {
        vec![ResolvedSegment::text_full(text_segment)]
    } else {
        segments
    }
}

fn push_token<F>(
    lookup: &F,
    text_segment: &str,
    start: usize,
    end: usize,
    segments: &mut Vec<ResolvedSegment>,
    last_emote_idx: &mut Option<usize>,
) where
    F: Fn(&str) -> Option<Arc<EmoteRef>>,
{
    if start >= end {
        return;
    }

    let token = &text_segment[start..end];
    if token.is_empty() {
        return;
    }

    if let Some(emote) = lookup(token) {
        if emote.is_modifier()
            && let Some(idx) = last_emote_idx.and_then(|idx| {
                if idx < segments.len() {
                    Some(idx)
                } else {
                    None
                }
            })
            && let ResolvedSegmentKind::Emote { modifiers, .. } = &mut segments[idx].kind
        {
            modifiers.push(ResolvedModifier::new(start..end, Arc::clone(&emote)));
            return;
        }

        let idx = segments.len();
        segments.push(ResolvedSegment::emote(start..end, emote));
        *last_emote_idx = Some(idx);
    } else {
        segments.push(ResolvedSegment::text(start..end));
        *last_emote_idx = None;
    }
}

#[derive(Default)]
pub struct EmojiResolver;

impl EmojiResolver {
    pub fn new() -> Self {
        EmojiResolver
    }

    pub fn shared() -> Arc<dyn EmoteResolver> {
        static INSTANCE: Lazy<Arc<dyn EmoteResolver>> =
            Lazy::new(|| Arc::new(EmojiResolver) as Arc<dyn EmoteResolver>);
        Arc::clone(&INSTANCE)
    }

    pub fn is_emoji_grapheme(grapheme: &str) -> bool {
        emojis::get(grapheme).is_some()
    }
}

impl EmoteResolver for EmojiResolver {
    fn resolve(&self, text_segment: &str) -> Vec<ResolvedSegment> {
        if text_segment.is_empty() {
            return Vec::new();
        }

        let mut segments = Vec::new();
        let mut cursor = 0;

        for (offset, grapheme) in text_segment.grapheme_indices(true) {
            let start = offset;
            let end = start + grapheme.len();

            if Self::is_emoji_grapheme(grapheme) {
                if cursor < start {
                    segments.push(ResolvedSegment::text(cursor..start));
                }
                let emote = Arc::new(EmoteRef::new(
                    EmoteId::new(EmoteProvider::Emoji, grapheme),
                    grapheme,
                    false,
                    Vec::new(),
                ));
                segments.push(ResolvedSegment::emote(start..end, emote));
                cursor = end;
            }
        }

        if cursor < text_segment.len() {
            segments.push(ResolvedSegment::text(cursor..text_segment.len()));
        }

        if segments.is_empty() {
            vec![ResolvedSegment::text_full(text_segment)]
        } else {
            segments
        }
    }
}

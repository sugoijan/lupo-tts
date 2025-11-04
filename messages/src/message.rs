use crate::emote::{EmoteRef, ResolvedSegmentKind, ResolverRegistry};
use std::ops::Range;
use std::sync::Arc;

pub type TextRange = Range<usize>;

#[derive(Clone, Debug, Default)]
pub struct Message {
    pub raw_text: String,
    pub segments: Vec<MessageSegment>,
}

impl Message {
    pub fn new(raw_text: String) -> Self {
        Self {
            raw_text,
            segments: Vec::new(),
        }
    }

    pub fn with_segments(raw_text: String, segments: Vec<MessageSegment>) -> Self {
        Self { raw_text, segments }
    }

    pub fn push_segment(&mut self, segment: MessageSegment) {
        self.segments.push(segment);
    }

    pub fn segment_text(&self, range: &TextRange) -> &str {
        &self.raw_text[range.clone()]
    }

    pub fn text_only(&self) -> String {
        let mut buffer = String::new();
        let mut pending_space = false;

        for segment in &self.segments {
            match segment {
                MessageSegment::Text { range } => {
                    let slice = self.segment_text(range);
                    for ch in slice.chars() {
                        if ch.is_whitespace() {
                            pending_space = !buffer.is_empty();
                        } else {
                            if pending_space && !buffer.ends_with(' ') {
                                buffer.push(' ');
                            }
                            pending_space = false;
                            buffer.push(ch);
                        }
                    }
                }
                MessageSegment::Emote { .. } | MessageSegment::Unknown { .. } => {
                    pending_space = !buffer.is_empty();
                }
            }
        }

        buffer.trim().to_string()
    }

    pub fn resolve_emotes(&mut self, channel_id: &str, registry: &ResolverRegistry) {
        if registry.is_empty() {
            return;
        }

        let mut updated = Vec::with_capacity(self.segments.len());
        let mut last_emote_idx: Option<usize> = None;

        for segment in self.segments.drain(..) {
            match segment {
                MessageSegment::Text { range } => {
                    let slice = &self.raw_text[range.clone()];
                    let resolved = registry.resolve(Some(channel_id), slice);
                    if resolved.is_empty() {
                        let is_whitespace = slice.chars().all(|ch| ch.is_whitespace());
                        updated.push(MessageSegment::Text {
                            range: range.clone(),
                        });
                        if !is_whitespace {
                            last_emote_idx = None;
                        }
                        continue;
                    }

                    for fragment in resolved {
                        let absolute_start = range.start + fragment.range.start;
                        let absolute_end = range.start + fragment.range.end;
                        let absolute_range = absolute_start..absolute_end;

                        match fragment.kind {
                            ResolvedSegmentKind::Text => {
                                let is_whitespace = self.raw_text[absolute_range.clone()]
                                    .chars()
                                    .all(|ch| ch.is_whitespace());
                                updated.push(MessageSegment::Text {
                                    range: absolute_range.clone(),
                                });
                                if !is_whitespace {
                                    last_emote_idx = None;
                                }
                            }
                            ResolvedSegmentKind::Unknown => {
                                updated.push(MessageSegment::Unknown {
                                    range: absolute_range.clone(),
                                });
                                last_emote_idx = None;
                            }
                            ResolvedSegmentKind::Emote { emote, modifiers } => {
                                if emote.is_modifier()
                                    && let Some(idx) = last_emote_idx
                                    && let MessageSegment::Emote {
                                        modifiers: target, ..
                                    } = &mut updated[idx]
                                {
                                    target.push(EmoteModifierInstance {
                                        range: absolute_range.clone(),
                                        emote: Arc::clone(&emote),
                                    });
                                    for modifier in modifiers {
                                        target.push(EmoteModifierInstance {
                                            range: range.start + modifier.range.start
                                                ..range.start + modifier.range.end,
                                            emote: modifier.emote,
                                        });
                                    }
                                    continue;
                                }

                                let converted_modifiers: Vec<EmoteModifierInstance> = modifiers
                                    .into_iter()
                                    .map(|modifier| EmoteModifierInstance {
                                        range: range.start + modifier.range.start
                                            ..range.start + modifier.range.end,
                                        emote: modifier.emote,
                                    })
                                    .collect();
                                updated.push(MessageSegment::Emote {
                                    range: absolute_range,
                                    emote,
                                    modifiers: converted_modifiers,
                                });
                                last_emote_idx = Some(updated.len() - 1);
                            }
                        }
                    }
                }
                MessageSegment::Emote {
                    range,
                    emote,
                    modifiers,
                } => {
                    updated.push(MessageSegment::Emote {
                        range,
                        emote,
                        modifiers,
                    });
                    last_emote_idx = Some(updated.len() - 1);
                }
                MessageSegment::Unknown { range } => {
                    updated.push(MessageSegment::Unknown { range });
                    last_emote_idx = None;
                }
            }
        }

        self.segments = updated;
    }
}

#[derive(Clone, Debug)]
pub enum MessageSegment {
    Text {
        range: TextRange,
    },
    Emote {
        range: TextRange,
        emote: Arc<EmoteRef>,
        modifiers: Vec<EmoteModifierInstance>,
    },
    Unknown {
        range: TextRange,
    },
}

#[derive(Clone, Debug)]
pub struct EmoteModifierInstance {
    pub range: TextRange,
    pub emote: Arc<EmoteRef>,
}

impl MessageSegment {
    pub fn range(&self) -> &TextRange {
        match self {
            MessageSegment::Text { range }
            | MessageSegment::Emote { range, .. }
            | MessageSegment::Unknown { range } => range,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emote::{EmoteResolver, resolve_with_lookup};
    use crate::{
        EmojiResolver, EmoteId, EmoteModifier, EmoteModifierFlags, EmoteProvider, ResolvedSegment,
    };
    use std::collections::HashMap;

    const CHANNEL_ID: &str = "channel-1";

    #[test]
    fn attaches_modifiers_to_preceding_emote() {
        let mut message = Message::with_segments(
            "Base ModOne ModTwo".to_string(),
            vec![MessageSegment::Text { range: 0..18 }],
        );
        let resolver = sample_resolver();
        let mut registry = ResolverRegistry::new();
        registry.add_channel(CHANNEL_ID, resolver);

        message.resolve_emotes(CHANNEL_ID, &registry);

        assert_eq!(message.segments.len(), 3);
        match &message.segments[0] {
            MessageSegment::Emote {
                range,
                emote,
                modifiers,
            } => {
                assert_eq!(range, &(0..4));
                assert_eq!(emote.code, "Base");
                assert_eq!(modifiers.len(), 2);
                assert_eq!(modifiers[0].range, 5..11);
                assert_eq!(modifiers[0].emote.code, "ModOne");
                assert_eq!(modifiers[1].range, 12..18);
                assert_eq!(modifiers[1].emote.code, "ModTwo");
            }
            other => panic!("expected emote segment, got {other:?}"),
        }
    }

    #[test]
    fn does_not_attach_modifier_after_text() {
        let mut message = Message::with_segments(
            "Base text ModOne".to_string(),
            vec![MessageSegment::Text { range: 0..16 }],
        );
        let resolver = sample_resolver();
        let mut registry = ResolverRegistry::new();
        registry.add_channel(CHANNEL_ID, resolver);

        message.resolve_emotes(CHANNEL_ID, &registry);

        let emote_count = message
            .segments
            .iter()
            .filter(|segment| matches!(segment, MessageSegment::Emote { .. }))
            .count();
        assert_eq!(emote_count, 2);

        let last_emote = message
            .segments
            .iter()
            .find(|segment| match segment {
                MessageSegment::Emote { range, .. } => range == &(10..16),
                _ => false,
            })
            .expect("modifier emote segment missing");

        if let MessageSegment::Emote { modifiers, .. } = last_emote {
            assert!(modifiers.is_empty(), "modifier should remain standalone");
        }
    }

    #[test]
    fn attaches_modifier_to_existing_emote_segment() {
        let base = base_emote();
        let mut message = Message::with_segments(
            "Base ModOne".to_string(),
            vec![
                MessageSegment::Emote {
                    range: 0..4,
                    emote: Arc::new(base),
                    modifiers: Vec::new(),
                },
                MessageSegment::Text { range: 4..11 },
            ],
        );
        let resolver = sample_resolver();
        let mut registry = ResolverRegistry::new();
        registry.add_channel(CHANNEL_ID, resolver);

        message.resolve_emotes(CHANNEL_ID, &registry);

        let emote_segment = &message.segments[0];
        match emote_segment {
            MessageSegment::Emote { modifiers, .. } => {
                assert_eq!(modifiers.len(), 1);
                assert_eq!(modifiers[0].range, 5..11);
                assert_eq!(modifiers[0].emote.code, "ModOne");
            }
            other => panic!("expected emote segment, got {other:?}"),
        }
    }

    fn sample_resolver() -> Arc<dyn EmoteResolver> {
        let base = base_emote();
        let mod_one = modifier_emote("ModOne", EmoteModifierFlags::RAINBOW);
        let mod_two = modifier_emote("ModTwo", EmoteModifierFlags::GROW_X);

        let mut resolver = StubResolver::default();
        resolver.insert(base);
        resolver.insert(mod_one);
        resolver.insert(mod_two);
        Arc::new(resolver) as Arc<dyn EmoteResolver>
    }

    #[test]
    fn resolves_standalone_emoji() {
        let raw = "Hello 😊 friend";
        let mut message = Message::with_segments(
            raw.to_string(),
            vec![MessageSegment::Text {
                range: 0..raw.len(),
            }],
        );
        let mut registry = ResolverRegistry::new();
        registry.add_global(EmojiResolver::shared());

        message.resolve_emotes(CHANNEL_ID, &registry);

        let emoji_segment = message
            .segments
            .iter()
            .find(|segment| {
                matches!(segment,
                    MessageSegment::Emote { emote, .. } if emote.code == "😊"
                )
            })
            .expect("emoji segment missing");
        match emoji_segment {
            MessageSegment::Emote { range, emote, .. } => {
                let start = raw.find('😊').unwrap();
                assert_eq!(range, &(start..start + "😊".len()));
                assert_eq!(emote.code, "😊");
                assert!(matches!(emote.id.provider, EmoteProvider::Emoji));
            }
            other => panic!("expected emoji emote, got {other:?}"),
        }
    }

    #[test]
    fn resolves_emoji_without_whitespace() {
        let text = "Goodnight🌙";
        let mut message = Message::with_segments(
            text.to_string(),
            vec![MessageSegment::Text {
                range: 0..text.len(),
            }],
        );
        let mut registry = ResolverRegistry::new();
        registry.add_global(EmojiResolver::shared());

        message.resolve_emotes(CHANNEL_ID, &registry);

        assert_eq!(message.segments.len(), 2);
        match &message.segments[1] {
            MessageSegment::Emote { range, emote, .. } => {
                let start = text.find('🌙').unwrap();
                assert_eq!(range, &(start..start + "🌙".len()));
                assert_eq!(emote.code, "🌙");
            }
            other => panic!("expected trailing emoji emote, got {other:?}"),
        }
    }

    #[test]
    fn resolves_multiple_consecutive_emojis() {
        let text = "😀😁";
        let mut message = Message::with_segments(
            text.to_string(),
            vec![MessageSegment::Text {
                range: 0..text.len(),
            }],
        );
        let mut registry = ResolverRegistry::new();
        registry.add_global(EmojiResolver::shared());

        message.resolve_emotes(CHANNEL_ID, &registry);

        assert_eq!(message.segments.len(), 2);
        match &message.segments[0] {
            MessageSegment::Emote { range, emote, .. } => {
                const FIRST: &str = "😀";
                let first_len = FIRST.len();
                assert_eq!(range, &(0..first_len));
                assert_eq!(emote.code, "😀");
            }
            other => panic!("expected first emoji, got {other:?}"),
        }
        match &message.segments[1] {
            MessageSegment::Emote { range, emote, .. } => {
                const SECOND: &str = "😁";
                let first_len = "😀".len();
                let second_len = SECOND.len();
                assert_eq!(range, &(first_len..first_len + second_len));
                assert_eq!(emote.code, "😁");
            }
            other => panic!("expected second emoji, got {other:?}"),
        }
    }

    #[test]
    fn resolves_skin_tone_modifier_sequence() {
        let text = "👍🏽";
        let mut message = Message::with_segments(
            text.to_string(),
            vec![MessageSegment::Text {
                range: 0..text.len(),
            }],
        );
        let mut registry = ResolverRegistry::new();
        registry.add_global(EmojiResolver::shared());

        message.resolve_emotes(CHANNEL_ID, &registry);

        assert_eq!(message.segments.len(), 1);
        match &message.segments[0] {
            MessageSegment::Emote { range, emote, .. } => {
                assert_eq!(range, &(0..text.len()));
                assert_eq!(emote.code, "👍🏽");
            }
            other => panic!("expected base emoji, got {other:?}"),
        }
    }

    #[test]
    fn text_only_collapse_spaces_between_removed_emotes() {
        let raw = "Hello Base Base world";
        let mut message = Message::with_segments(
            raw.to_string(),
            vec![MessageSegment::Text {
                range: 0..raw.len(),
            }],
        );

        let resolver = sample_resolver();
        let mut registry = ResolverRegistry::new();
        registry.add_channel(CHANNEL_ID, resolver);
        message.resolve_emotes(CHANNEL_ID, &registry);

        assert_eq!(message.text_only(), "Hello world");
    }

    fn base_emote() -> EmoteRef {
        EmoteRef::new(
            EmoteId::new(EmoteProvider::FrankerFaceZ, "base"),
            "Base",
            false,
            Vec::new(),
        )
    }

    fn modifier_emote(code: &str, flag: EmoteModifierFlags) -> EmoteRef {
        let modifier = EmoteModifier {
            flags: flag,
            raw_flags: flag.bits(),
            is_hidden: flag.contains(EmoteModifierFlags::HIDDEN),
            mask_images: Vec::new(),
        };
        EmoteRef::new(
            EmoteId::new(EmoteProvider::FrankerFaceZ, code.to_lowercase()),
            code,
            false,
            Vec::new(),
        )
        .with_modifier(modifier)
    }

    #[derive(Default)]
    struct StubResolver {
        emotes: HashMap<String, Arc<EmoteRef>>,
    }

    impl StubResolver {
        fn insert(&mut self, emote: EmoteRef) {
            self.emotes.insert(emote.code.clone(), Arc::new(emote));
        }
    }

    impl EmoteResolver for StubResolver {
        fn resolve(&self, text_segment: &str) -> Vec<ResolvedSegment> {
            resolve_with_lookup(text_segment, |token| self.emotes.get(token).cloned())
        }
    }
}

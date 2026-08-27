// SPDX-License-Identifier: Apache-2.0

//! [`AudioSink`] implementation that routes the pipeline's speech to every live
//! `lupo_tts` source instance in OBS.
//!
//! Each source instance registers its [`Player`] here on create and removes it
//! on destroy. A clip is pushed to all of them; OBS only mixes the instances
//! that are active (visible in the program scene, or claimed into a global
//! output channel), so this is what makes "place the source wherever you want
//! it heard" work without the sink knowing about scenes.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use lupo_audio::{AudioSink, Error, PcmBuffer, SinkFuture, clamp_volume, decode_to_pcm};
use tokio::sync::{Notify, oneshot};
use tracing::{debug, warn};

use crate::audio::Player;

/// Identifies a registered source instance (the address of its private data).
pub type TargetId = usize;

/// How long playback waits for a `lupo_tts` source instance to appear before
/// failing. Covers OBS startup, where the pipeline (started in
/// `obs_module_post_load`) resumes persisted messages before the scene
/// collection has created any source instance.
const TARGET_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct ObsSink {
    targets: Mutex<BTreeMap<TargetId, Arc<Player>>>,
    /// Current linear gain, as `f32` bits; applied live by every [`Player`].
    gain_bits: AtomicU32,
    /// Woken whenever a target registers, so playback waiting in
    /// [`ObsSink::wait_for_target`] can proceed.
    targets_changed: Notify,
    /// Completion receivers for clips handed to players but not yet drained.
    pending: Mutex<Vec<oneshot::Receiver<()>>>,
}

impl Default for ObsSink {
    fn default() -> Self {
        Self {
            targets: Mutex::new(BTreeMap::new()),
            gain_bits: AtomicU32::new(1.0f32.to_bits()),
            targets_changed: Notify::new(),
            pending: Mutex::new(Vec::new()),
        }
    }
}

impl ObsSink {
    pub fn register(&self, id: TargetId, player: Arc<Player>) {
        player.set_gain(self.gain());
        let count = {
            let mut targets = self.targets.lock().unwrap_or_else(|e| e.into_inner());
            targets.insert(id, player);
            targets.len()
        };
        self.targets_changed.notify_waiters();
        debug!(id, count, "sink target registered");
    }

    pub fn unregister(&self, id: TargetId) {
        let count = {
            let mut targets = self.targets.lock().unwrap_or_else(|e| e.into_inner());
            targets.remove(&id);
            targets.len()
        };
        debug!(id, count, "sink target unregistered");
    }

    pub fn target_count(&self) -> usize {
        self.targets.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn gain(&self) -> f32 {
        f32::from_bits(self.gain_bits.load(Ordering::Relaxed))
    }

    fn players(&self) -> Vec<Arc<Player>> {
        self.targets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    fn dispatch(&self, pcm: PcmBuffer) -> Result<(), Error> {
        let players = self.players();
        if players.is_empty() {
            return Err(Error::Sink(
                "no LUPO TTS source exists in OBS; add one to a scene".to_string(),
            ));
        }
        let pcm = Arc::new(pcm);
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        for player in players {
            pending.push(player.play(pcm.clone()));
        }
        Ok(())
    }

    /// Wait (bounded by [`TARGET_WAIT`]) until at least one source instance is
    /// registered. On timeout returns anyway and lets [`ObsSink::dispatch`]
    /// report the missing source.
    async fn wait_for_target(&self) {
        let deadline = tokio::time::Instant::now() + TARGET_WAIT;
        loop {
            let notified = self.targets_changed.notified();
            if self.target_count() > 0 {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return;
            }
        }
    }
}

impl AudioSink for ObsSink {
    fn play_wav(&self, audio_data: Bytes) -> SinkFuture<'_, ()> {
        Box::pin(async move {
            let pcm = decode_to_pcm(audio_data)?;
            self.wait_for_target().await;
            self.dispatch(pcm)
        })
    }

    fn stop(&self) -> SinkFuture<'_, ()> {
        Box::pin(async move {
            for player in self.players() {
                player.stop();
            }
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            Ok(())
        })
    }

    fn drain(&self) -> SinkFuture<'_, ()> {
        Box::pin(async move {
            let receivers =
                std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
            for receiver in receivers {
                // A dropped sender (stop / pre-empt / destroy) also counts as done.
                if receiver.await.is_err() {
                    warn!("clip ended early (stopped, pre-empted, or source destroyed)");
                }
            }
            Ok(())
        })
    }

    fn set_volume(&self, volume: f32) -> SinkFuture<'_, f32> {
        let clamped = clamp_volume(volume);
        self.gain_bits.store(clamped.to_bits(), Ordering::Relaxed);
        // Applied live so the clip currently playing follows the slider.
        for player in self.players() {
            player.set_gain(clamped);
        }
        Box::pin(async move { Ok(clamped) })
    }

    fn volume(&self) -> SinkFuture<'_, f32> {
        let gain = self.gain();
        Box::pin(async move { Ok(gain) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_without_targets_is_an_error() {
        let sink = ObsSink::default();
        let result = sink
            .play_wav(Bytes::from_static(b"not audio"))
            .await
            .unwrap_err();
        // Decoding fails before target lookup for garbage input...
        assert!(matches!(result, Error::Decode(_)));
        // ...and a decodable buffer with no targets reports the missing source.
        let pcm = crate::audio::tone(440.0, 0.01, 8_000, 0.1);
        assert!(matches!(sink.dispatch(pcm), Err(Error::Sink(_))));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn volume_round_trips_and_clamps() {
        let sink = ObsSink::default();
        assert_eq!(sink.set_volume(3.0).await.unwrap(), lupo_audio::MAX_VOLUME);
        assert_eq!(sink.volume().await.unwrap(), lupo_audio::MAX_VOLUME);
        assert_eq!(sink.target_count(), 0);
    }
}

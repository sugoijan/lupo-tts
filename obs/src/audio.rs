// SPDX-License-Identifier: Apache-2.0

//! Push decoded PCM into an OBS source in real time.
//!
//! OBS audio sources are *push* model: we hand `obs_source_output_audio` chunks
//! with timestamps and OBS buffers, resamples and mixes them. libobs treats a
//! timestamp within ~2 s of `os_gettime_ns()` as a "direct" timestamp, so the
//! [`Player`] thread paces itself against the libobs clock and stamps each
//! chunk with the time it is meant to play at, pushing slightly ahead so OBS
//! never starves.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use tokio::sync::oneshot;

use lupo_audio::PcmBuffer;
use lupo_obs_sys as sys;
use tracing::{debug, info, warn};

/// Chunk length handed to OBS per push.
const CHUNK_MS: u64 = 10;
/// How far ahead of its timestamp a chunk is pushed.
const LEAD_NS: u64 = 20_000_000;
const NS_PER_SEC: u64 = 1_000_000_000;

/// `obs_source_t*` that can live on the player thread. libobs guarantees
/// `obs_source_output_audio` is safe from any thread while the source is
/// alive; [`Player`] is joined in the source's `destroy` so the pointer never
/// outlives the source.
#[derive(Clone, Copy)]
pub struct SourceHandle(pub *mut sys::obs_source_t);

// SAFETY: see the type-level comment; libobs source handles are thread-safe
// reference-counted objects and the owner controls the lifetime.
unsafe impl Send for SourceHandle {}
unsafe impl Sync for SourceHandle {}

/// Counters shared between the player thread, the OBS audio capture callback
/// and log lines. Frames are in the clip's sample rate for `pushed` and in
/// OBS's output rate for `consumed`.
#[derive(Debug, Default)]
pub struct Stats {
    pub pushed_frames: AtomicU64,
    pub consumed_frames: AtomicU64,
    pub consumed_muted_frames: AtomicU64,
    pub clips_played: AtomicU64,
}

enum Command {
    /// Play a clip; the sender is dropped (or completed) when the clip ends
    /// for any reason. The buffer is shared so the sink can fan one clip out
    /// to every player without copying the samples.
    Play(Arc<PcmBuffer>, oneshot::Sender<()>),
    Stop,
    Shutdown,
}

#[derive(Debug)]
pub struct Player {
    tx: Sender<Command>,
    thread: Mutex<Option<JoinHandle<()>>>,
    stats: Arc<Stats>,
    /// Linear gain applied per chunk, as `f32` bits; changes take effect
    /// mid-clip.
    gain_bits: Arc<AtomicU32>,
}

impl Player {
    pub fn spawn(source: SourceHandle, label: String) -> Self {
        let (tx, rx) = mpsc::channel();
        let stats = Arc::new(Stats::default());
        let thread_stats = stats.clone();
        let gain_bits = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let thread_gain = gain_bits.clone();
        let thread = thread::Builder::new()
            .name(format!("lupo-obs-player:{label}"))
            .spawn(move || run(source, rx, thread_stats, thread_gain, label))
            .expect("failed to spawn player thread");
        Self {
            tx,
            thread: Mutex::new(Some(thread)),
            stats,
            gain_bits,
        }
    }

    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }

    /// Set the linear gain applied to pushed samples; affects the clip that is
    /// currently playing from its next chunk on.
    pub fn set_gain(&self, gain: f32) {
        self.gain_bits.store(gain.to_bits(), Ordering::Relaxed);
    }

    /// Enqueue a clip. The returned receiver resolves `Ok(())` when the clip
    /// played to the end and `Err` if it was stopped, pre-empted, or the player
    /// shut down.
    pub fn play(&self, pcm: Arc<PcmBuffer>) -> oneshot::Receiver<()> {
        let (done, receiver) = oneshot::channel();
        let _ = self.tx.send(Command::Play(pcm, done));
        receiver
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Command::Stop);
    }

    /// Stop the player thread and join it. Must be called (directly or via
    /// drop) before the source pointer dies: other `Arc<Player>` clones may
    /// still be around (e.g. briefly held by a concurrent sink dispatch), so
    /// the owner must not rely on dropping the last reference. Idempotent.
    pub fn shutdown(&self) {
        let _ = self.tx.send(Command::Shutdown);
        let thread = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shutdown();
    }
}

enum Outcome {
    /// The clip ended (played out, stopped, or skipped); wait for the next command.
    Finished,
    /// A new command arrived mid-clip and must be processed next.
    Interrupted(Command),
    Shutdown,
}

fn run(
    source: SourceHandle,
    rx: Receiver<Command>,
    stats: Arc<Stats>,
    gain_bits: Arc<AtomicU32>,
    label: String,
) {
    debug!(source = %label, "player thread started");
    let mut next = rx.recv().ok();
    while let Some(command) = next.take() {
        match command {
            Command::Play(pcm, done) => {
                match play_clip(source, &rx, &stats, &gain_bits, &label, &pcm) {
                    Outcome::Finished => {
                        let _ = done.send(());
                        next = rx.recv().ok();
                    }
                    Outcome::Interrupted(command) => {
                        drop(done);
                        next = Some(command);
                    }
                    Outcome::Shutdown => break,
                }
            }
            Command::Stop => next = rx.recv().ok(),
            Command::Shutdown => break,
        }
    }
    debug!(source = %label, "player thread exiting");
}

/// Push one clip, chunk by chunk, in real time.
fn play_clip(
    source: SourceHandle,
    rx: &Receiver<Command>,
    stats: &Stats,
    gain_bits: &AtomicU32,
    label: &str,
    pcm: &PcmBuffer,
) -> Outcome {
    let speakers = match pcm.channels {
        1 => sys::SPEAKERS_MONO,
        2 => sys::SPEAKERS_STEREO,
        other => {
            warn!(
                source = label,
                channels = other,
                "unsupported channel count; clip skipped"
            );
            return Outcome::Finished;
        }
    };
    let channels = usize::from(pcm.channels);
    let chunk_frames = (u64::from(pcm.sample_rate) * CHUNK_MS / 1000).max(1) as usize;
    let total_frames = pcm.frames();

    // SAFETY: plain FFI call with no arguments.
    let start_ns = unsafe { sys::os_gettime_ns() } + LEAD_NS;
    let mut frame_index = 0usize;
    let mut chunk_index = 0u64;
    let mut scaled: Vec<f32> = Vec::new();

    while frame_index < total_frames {
        match rx.try_recv() {
            Ok(Command::Stop) => {
                info!(source = label, frame_index, total_frames, "clip stopped");
                return Outcome::Interrupted(Command::Stop);
            }
            Ok(Command::Shutdown) | Err(TryRecvError::Disconnected) => return Outcome::Shutdown,
            Ok(command @ Command::Play(..)) => {
                // A new clip pre-empts the current one. The pipeline serialises
                // playback, so this only happens for the test buttons.
                info!(source = label, "clip pre-empted by a new clip");
                return Outcome::Interrupted(command);
            }
            Err(TryRecvError::Empty) => {}
        }

        let frames = chunk_frames.min(total_frames - frame_index);
        let timestamp =
            start_ns + chunk_index * chunk_frames as u64 * NS_PER_SEC / u64::from(pcm.sample_rate);
        // SAFETY: plain FFI call.
        unsafe { sys::os_sleepto_ns(timestamp.saturating_sub(LEAD_NS)) };

        let chunk = &pcm.samples[frame_index * channels..(frame_index + frames) * channels];
        let gain = f32::from_bits(gain_bits.load(Ordering::Relaxed));
        let samples: &[f32] = if (gain - 1.0).abs() > f32::EPSILON {
            scaled.clear();
            scaled.extend(chunk.iter().map(|sample| sample * gain));
            &scaled
        } else {
            chunk
        };
        let mut audio = sys::obs_source_audio {
            data: [std::ptr::null(); sys::MAX_AV_PLANES as usize],
            frames: frames as u32,
            speakers,
            format: sys::AUDIO_FORMAT_FLOAT,
            samples_per_sec: pcm.sample_rate,
            timestamp,
        };
        audio.data[0] = samples.as_ptr().cast();
        // SAFETY: `source` outlives this thread (joined in destroy); `audio`
        // and the sample slice are valid for the duration of the call, and
        // libobs copies the data before returning.
        unsafe { sys::obs_source_output_audio(source.0, &audio) };
        stats
            .pushed_frames
            .fetch_add(frames as u64, Ordering::Relaxed);

        frame_index += frames;
        chunk_index += 1;
    }

    // Let the tail reach OBS's mixer before reporting.
    let end_ns = start_ns + total_frames as u64 * NS_PER_SEC / u64::from(pcm.sample_rate);
    // SAFETY: plain FFI call.
    unsafe { sys::os_sleepto_ns(end_ns + 50_000_000) };
    stats.clips_played.fetch_add(1, Ordering::Relaxed);
    info!(
        source = label,
        pushed_frames = stats.pushed_frames.load(Ordering::Relaxed),
        consumed_frames = stats.consumed_frames.load(Ordering::Relaxed),
        consumed_muted_frames = stats.consumed_muted_frames.load(Ordering::Relaxed),
        clip_ms = pcm.duration().as_millis() as u64,
        sample_rate = pcm.sample_rate,
        channels = pcm.channels,
        "clip finished"
    );
    Outcome::Finished
}

/// A mono sine tone; used by the "test tone" button.
pub fn tone(frequency_hz: f32, seconds: f32, sample_rate: u32, amplitude: f32) -> PcmBuffer {
    let frames = (seconds * sample_rate as f32) as usize;
    let samples = (0..frames)
        .map(|i| {
            let t = i as f32 / sample_rate as f32;
            // 10 ms fade in/out to avoid clicks.
            let fade = (t * 100.0).min(1.0).min((seconds - t) * 100.0).max(0.0);
            amplitude * fade * (2.0 * std::f32::consts::PI * frequency_hz * t).sin()
        })
        .collect();
    PcmBuffer {
        sample_rate,
        channels: 1,
        samples,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_has_expected_length_and_fades() {
        let pcm = tone(440.0, 0.5, 48_000, 0.5);
        assert_eq!(pcm.frames(), 24_000);
        assert_eq!(pcm.channels, 1);
        assert_eq!(pcm.samples[0], 0.0);
        assert!(pcm.samples.iter().all(|s| s.abs() <= 0.5));
        assert!(pcm.samples.iter().any(|s| s.abs() > 0.4));
    }
}

// SPDX-License-Identifier: Apache-2.0

//! Audio output abstraction for LUPO frontends.
//!
//! The pipeline talks to an [`AudioSink`]; each frontend supplies the
//! implementation that fits its host:
//!
//! - [`DeviceSink`] (feature `device`, on by default) plays through the default
//!   output device via rodio. The desktop app uses this.
//! - Hosts that own the audio path (e.g. an OBS source that pushes PCM frames
//!   into the host mixer) implement [`AudioSink`] themselves and use
//!   [`decode_to_pcm`] to turn synthesized audio bytes into raw samples.

use bytes::Bytes;
use rodio::Source;
use rodio::decoder::DecoderError;
use std::fmt;
use std::future::Future;
use std::io::BufReader;
use std::pin::Pin;
use std::time::Duration;
use thiserror::Error;

pub const MIN_VOLUME: f32 = 0.0;
pub const MAX_VOLUME: f32 = 2.0;

#[derive(Debug, Error)]
pub enum Error {
    #[cfg(feature = "device")]
    #[error("failed to open audio output stream: {0}")]
    Stream(#[from] rodio::DeviceSinkError),

    #[error("failed to decode audio: {0}")]
    Decode(#[from] DecoderError),

    #[error("audio sink error: {0}")]
    Sink(String),

    #[error("command channel closed unexpectedly")]
    ChannelClosed,

    #[error("oneshot channel dropped before response")]
    ResponseDropped,
}

/// Boxed future returned by [`AudioSink`] methods so the trait stays object-safe.
pub type SinkFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

/// Destination for synthesized speech.
///
/// All methods are asynchronous and must be safe to call from any task; the
/// sink owns whatever thread or device state it needs. Volume is a linear gain
/// in `MIN_VOLUME..=MAX_VOLUME`; implementations clamp with [`clamp_volume`].
pub trait AudioSink: Send + Sync + fmt::Debug {
    /// Enqueue an encoded audio clip (WAV or any format rodio can decode) and
    /// return once it has been accepted, not once it has finished playing.
    fn play_wav(&self, audio_data: Bytes) -> SinkFuture<'_, ()>;

    /// Enqueue a clip and resolve once everything queued so far has played.
    fn play_wav_and_wait(&self, audio_data: Bytes) -> SinkFuture<'_, ()> {
        Box::pin(async move {
            self.play_wav(audio_data).await?;
            self.drain().await
        })
    }

    /// Stop playback immediately and discard anything queued.
    fn stop(&self) -> SinkFuture<'_, ()>;

    /// Resolve once everything queued so far has played.
    fn drain(&self) -> SinkFuture<'_, ()>;

    /// Set the output gain; resolves with the effective (clamped) value.
    fn set_volume(&self, volume: f32) -> SinkFuture<'_, f32>;

    /// Current output gain.
    fn volume(&self) -> SinkFuture<'_, f32>;
}

/// Decoded, interleaved 32-bit float PCM.
#[derive(Clone, Debug, PartialEq)]
pub struct PcmBuffer {
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved samples in `-1.0..=1.0`, `channels` values per frame.
    pub samples: Vec<f32>,
}

impl PcmBuffer {
    pub fn frames(&self) -> usize {
        self.samples.len() / usize::from(self.channels.max(1))
    }

    pub fn duration(&self) -> Duration {
        if self.sample_rate == 0 {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(self.frames() as f64 / f64::from(self.sample_rate))
    }
}

/// Decode an encoded clip into raw PCM without touching any output device.
///
/// Hosts that push samples themselves (OBS sources, tests) use this instead of
/// [`DeviceSink`].
pub fn decode_to_pcm(audio_data: impl Into<Bytes>) -> Result<PcmBuffer, Error> {
    let cursor = std::io::Cursor::new(audio_data.into());
    let decoder = rodio::Decoder::try_from(BufReader::new(cursor))?;
    let sample_rate = decoder.sample_rate().get();
    let channels = decoder.channels().get();
    let samples: Vec<f32> = decoder.collect();
    Ok(PcmBuffer {
        sample_rate,
        channels,
        samples,
    })
}

pub fn clamp_volume(value: f32) -> f32 {
    value.clamp(MIN_VOLUME, MAX_VOLUME)
}

#[cfg(feature = "device")]
mod device {
    use super::{AudioSink, Error, SinkFuture, clamp_volume};
    use bytes::Bytes;
    use rodio::{Decoder, DeviceSinkBuilder, Player};
    use std::io::BufReader;
    use std::sync::mpsc::{self, Sender};
    use std::thread;
    use tokio::sync::oneshot;
    use tracing::*;

    enum AudioCommand {
        PlayWav {
            audio_data: Bytes,
            resp: oneshot::Sender<Result<(), Error>>,
        },
        Stop {
            resp: oneshot::Sender<Result<(), Error>>,
        },
        Drain {
            resp: oneshot::Sender<Result<(), Error>>,
        },
        SetVolume {
            volume: f32,
            resp: oneshot::Sender<Result<f32, Error>>,
        },
        GetVolume {
            resp: oneshot::Sender<Result<f32, Error>>,
        },
    }

    /// [`AudioSink`] backed by the default output device (rodio/cpal).
    ///
    /// This is just a handle around a command channel; the device and player
    /// live on a dedicated thread.
    #[derive(Debug)]
    pub struct DeviceSink {
        sender: Sender<AudioCommand>,
    }

    impl DeviceSink {
        pub fn start_default() -> Result<Self, Error> {
            // The thread reports whether it managed to open the stream before
            // `start_default` returns.
            let (tx_init, rx_init) = mpsc::channel::<Result<(), Error>>();
            let (tx_cmd, rx_cmd) = mpsc::channel::<AudioCommand>();

            thread::spawn(move || {
                let stream = match DeviceSinkBuilder::open_default_sink() {
                    Ok(stream) => stream,
                    Err(e) => {
                        let _ = tx_init.send(Err(e.into()));
                        return;
                    }
                };
                let mut sink = Player::connect_new(stream.mixer());
                let mut volume = 1.0_f32;
                sink.set_volume(volume);

                let _ = tx_init.send(Ok(()));

                for cmd in rx_cmd {
                    match cmd {
                        AudioCommand::PlayWav { audio_data, resp } => {
                            let result: Result<(), Error> = (|| {
                                let cursor = std::io::Cursor::new(audio_data);
                                let reader = BufReader::new(cursor);
                                let source = Decoder::try_from(reader)?;
                                sink.append(source);
                                Ok(())
                            })();
                            let _ = resp.send(result);
                        }
                        AudioCommand::Stop { resp } => {
                            // Stop current playback and recreate the player so
                            // future appends are accepted on a fresh queue.
                            sink.stop();
                            sink = Player::connect_new(stream.mixer());
                            sink.set_volume(volume);
                            let _ = resp.send(Ok(()));
                        }
                        AudioCommand::Drain { resp } => {
                            sink.sleep_until_end();
                            let _ = resp.send(Ok(()));
                        }
                        AudioCommand::SetVolume { volume: next, resp } => {
                            volume = clamp_volume(next);
                            sink.set_volume(volume);
                            let _ = resp.send(Ok(volume));
                        }
                        AudioCommand::GetVolume { resp } => {
                            let _ = resp.send(Ok(volume));
                        }
                    }
                }

                drop(sink);
                drop(stream);
                trace!("Audio thread: channel closed, exiting.");
            });

            let audio = DeviceSink { sender: tx_cmd };
            rx_init
                .recv()
                .map_err(|_| Error::ChannelClosed)
                .and_then(|i| i)
                .map(|_| audio)
        }

        // The command is sent when the future is first polled, not when it is
        // created: a caller that builds the future and drops it unpolled must
        // not have queued work on the audio thread.
        fn request<'a, T: Send + 'static>(
            &'a self,
            make: impl FnOnce(oneshot::Sender<Result<T, Error>>) -> AudioCommand + Send + 'a,
        ) -> SinkFuture<'a, T> {
            Box::pin(async move {
                let (tx_reply, rx_reply) = oneshot::channel::<Result<T, Error>>();
                self.sender
                    .send(make(tx_reply))
                    .map_err(|_| Error::ChannelClosed)?;
                match rx_reply.await {
                    Ok(result) => result,
                    Err(_) => Err(Error::ResponseDropped),
                }
            })
        }
    }

    impl AudioSink for DeviceSink {
        fn play_wav(&self, audio_data: Bytes) -> SinkFuture<'_, ()> {
            self.request(|resp| AudioCommand::PlayWav { audio_data, resp })
        }

        fn stop(&self) -> SinkFuture<'_, ()> {
            self.request(|resp| AudioCommand::Stop { resp })
        }

        fn drain(&self) -> SinkFuture<'_, ()> {
            self.request(|resp| AudioCommand::Drain { resp })
        }

        fn set_volume(&self, volume: f32) -> SinkFuture<'_, f32> {
            self.request(move |resp| AudioCommand::SetVolume { volume, resp })
        }

        fn volume(&self) -> SinkFuture<'_, f32> {
            self.request(|resp| AudioCommand::GetVolume { resp })
        }
    }
}

#[cfg(feature = "device")]
pub use device::DeviceSink;

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal 16-bit PCM WAV in memory.
    fn wav_pcm16(sample_rate: u32, channels: u16, frames: &[i16]) -> Vec<u8> {
        let data_len = (frames.len() * 2) as u32;
        let block_align = channels * 2;
        let mut out = Vec::with_capacity(44 + data_len as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
        out.extend_from_slice(&block_align.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for sample in frames {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        out
    }

    #[test]
    fn clamp_volume_enforces_bounds() {
        assert_eq!(clamp_volume(-1.0), MIN_VOLUME);
        assert_eq!(clamp_volume(0.5), 0.5);
        assert_eq!(clamp_volume(4.0), MAX_VOLUME);
    }

    #[test]
    fn decode_to_pcm_yields_interleaved_float_samples() {
        // Two stereo frames: (max, min), (0, half).
        let wav = wav_pcm16(8_000, 2, &[i16::MAX, i16::MIN, 0, i16::MAX / 2]);
        let pcm = decode_to_pcm(wav).expect("decode should succeed");
        assert_eq!(pcm.sample_rate, 8_000);
        assert_eq!(pcm.channels, 2);
        assert_eq!(pcm.frames(), 2);
        assert_eq!(pcm.samples.len(), 4);
        assert!((pcm.samples[0] - 1.0).abs() < 1e-3);
        assert!((pcm.samples[1] + 1.0).abs() < 1e-3);
        assert!(pcm.samples[2].abs() < 1e-3);
        assert!((pcm.samples[3] - 0.5).abs() < 1e-3);
        assert_eq!(pcm.duration(), Duration::from_micros(250));
    }

    #[test]
    fn decode_to_pcm_rejects_garbage() {
        assert!(matches!(
            decode_to_pcm(vec![0u8; 16]),
            Err(Error::Decode(_))
        ));
    }

    #[cfg(feature = "device")]
    #[tokio::test(flavor = "multi_thread")]
    async fn set_volume_round_trip_when_output_device_available() {
        let Ok(audio) = DeviceSink::start_default() else {
            // Some CI environments have no default output device.
            return;
        };

        let expected = 1.35_f32;
        let effective = audio
            .set_volume(expected)
            .await
            .expect("set volume should succeed");
        assert!((effective - expected).abs() < 0.001);

        let current = audio.volume().await.expect("get volume should succeed");
        assert!((current - expected).abs() < 0.001);
    }
}

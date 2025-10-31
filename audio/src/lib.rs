// SPDX-License-Identifier: Apache-2.0

use bytes::Bytes;
use rodio::decoder::DecoderError;
use rodio::{Decoder, OutputStreamBuilder, Sink, StreamError};
use std::io::BufReader;
use std::sync::mpsc::{self, Sender};
use std::thread;
use thiserror::Error;
use tokio::sync::oneshot;
use tracing::*;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to open audio output stream: {0}")]
    Stream(#[from] StreamError),

    #[error("failed to decode audio: {0}")]
    Decode(#[from] DecoderError),

    #[error("command channel closed unexpectedly")]
    ChannelClosed,

    #[error("oneshot channel dropped before response")]
    ResponseDropped,
}

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
}

// `Audio` is just a handle (wrapper around `Sender<AudioCommand>`).
#[derive(Debug)]
pub struct Audio {
    sender: Sender<AudioCommand>,
}

impl Audio {
    pub fn start_default() -> Result<Self, Error> {
        // We'll create a (tx_init, rx_init) pair so that the thread
        // can tell us “OK, I opened the stream successfully” or “Err(…)”
        // before we return from start_default().
        let (tx_init, rx_init) = mpsc::channel::<Result<(), Error>>();

        // This is the channel we actually use for PlayWav/Stop commands.
        let (tx_cmd, rx_cmd) = mpsc::channel::<AudioCommand>();

        // Now spawn the audio thread.
        thread::spawn(move || -> () {
            // 1) Try to open the default audio device on THIS thread:
            let stream = match OutputStreamBuilder::open_default_stream() {
                Ok(stream) => stream,
                Err(e) => {
                    let _ = tx_init.send(Err(e.into()));
                    return;
                }
            };
            // a) Build the Sink attached to the stream's mixer.
            let sink = Sink::connect_new(stream.mixer());

            // Tell the caller: “both stream + sink opened fine”
            let _ = tx_init.send(Ok(()));

            // 2) Now loop over incoming commands.
            for cmd in rx_cmd {
                match cmd {
                    AudioCommand::PlayWav { audio_data, resp } => {
                        // Try to decode the wav bytes:
                        let result: Result<(), Error> = (|| {
                            let cursor = std::io::Cursor::new(audio_data);
                            let reader = BufReader::new(cursor);
                            let source = Decoder::try_from(reader)?;
                            sink.append(source);
                            Ok(())
                        })();

                        // Send back success or decode‐error:
                        let _ = resp.send(result);
                    }
                    AudioCommand::Stop { resp } => {
                        // Stopping never panics: stop() returns ()
                        sink.stop();
                        let _ = resp.send(Ok(()));
                    }
                    AudioCommand::Drain { resp } => {
                        sink.sleep_until_end();
                        let _ = resp.send(Ok(()));
                    }
                }
            }

            // If we get here, it means `tx_cmd` was dropped
            // (all senders closed). Fall out of the loop:
            drop(sink);
            drop(stream);
            trace!("Audio thread: channel closed, exiting.");
        });

        // Meanwhile, **block** until the thread has told us whether
        // it succeeded in opening the stream+sink or not:
        let audio = Audio { sender: tx_cmd };
        rx_init
            .recv()
            .map_err(|_| Error::ChannelClosed)
            .and_then(|i| i)
            .map(|_| audio)
    }

    pub async fn play_wav(&self, audio_data: impl Into<Bytes>) -> Result<(), Error> {
        let (tx_reply, rx_reply) = oneshot::channel::<Result<(), Error>>();
        let cmd = AudioCommand::PlayWav {
            audio_data: audio_data.into(),
            resp: tx_reply,
        };
        self.sender.send(cmd).map_err(|_| Error::ChannelClosed)?;
        match rx_reply.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(Error::ResponseDropped),
        }
    }

    pub async fn stop(&self) -> Result<(), Error> {
        let (tx_reply, rx_reply) = oneshot::channel::<Result<(), Error>>();
        let cmd = AudioCommand::Stop { resp: tx_reply };
        self.sender.send(cmd).map_err(|_| Error::ChannelClosed)?;
        match rx_reply.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(Error::ResponseDropped),
        }
    }

    pub async fn drain(&self) -> Result<(), Error> {
        let (tx_reply, rx_reply) = oneshot::channel::<Result<(), Error>>();
        let cmd = AudioCommand::Drain { resp: tx_reply };
        self.sender.send(cmd).map_err(|_| Error::ChannelClosed)?;
        match rx_reply.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(Error::ResponseDropped),
        }
    }
}

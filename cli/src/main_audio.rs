// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::io::BufReader;
use rodio::{Decoder, OutputStream, Sink};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Open the file on disk
    let file = File::open("speech_nice.mp3")
        .map_err(|e| format!("Failed to open file: {}", e))?;
    let reader = BufReader::new(file);

    // 2. Create an OutputStream and handle to control sinks
    //    This also initializes the default audio device via CPAL.
    let (_stream, stream_handle) = OutputStream::try_default()
        .map_err(|e| format!("Failed to open audio output: {}", e))?;

    // 3. Create a Sink: this is your playback handle.
    let sink = Sink::try_new(&stream_handle)
        .map_err(|e| format!("Failed to create sink: {}", e))?;

    // 4. Decode the MP3 data into a Source of PCM samples.
    let source = Decoder::new(reader)
        .map_err(|e| format!("Failed to decode MP3: {}", e))?;

    // 5. Append the source to the sink; playback begins immediately in a background thread.
    sink.append(source);

    // 6. Block the main thread until the audio is done.
    sink.sleep_until_end();

    Ok(())
}

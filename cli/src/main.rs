// SPDX-License-Identifier: Apache-2.0

use bytes::Bytes;
use reqwest::Client;
use rodio::{Decoder, OutputStream, Sink};
use serde::Deserialize;
use serde_json::json;
use std::env;
use std::io::{BufReader, Cursor};

#[derive(Deserialize)]
struct ErrorResponse {
    error: OpenAiError,
}
#[derive(Deserialize)]
struct OpenAiError {
    message: String,
    #[serde(default)]
    code: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Read the API key
    let api_key = env::var("OPENAI_API_KEY").expect("Please set the OPENAI_API_KEY env var");

    // 2. Construct client
    let client = Client::new();

    // 3. Prepare the request body
    let body = json!({
        "model": "gpt-4o-mini-tts",
        "voice": "alloy",           // choose your voice
        "input": "Ciao, come va?"  // your TTS text
    });

    // 4. Send the POST
    let resp = client
        .post("https://api.openai.com/v1/audio/speech")
        .bearer_auth(api_key)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    // 5. Handle non-200 errors
    let status = resp.status();
    if !status.is_success() {
        // attempt to parse OpenAI error payload
        if let Ok(err) = resp.json::<ErrorResponse>().await {
            panic!(
                "OpenAI API error ({}): {}",
                err.error.code.unwrap_or_default(),
                err.error.message
            );
        } else {
            panic!("Request failed with status: {}", status);
        }
    }

    // 6. Pull the bytes into memory
    let audio_bytes: Bytes = resp.bytes().await?;
    println!("Fetched {} bytes of OGG audio", audio_bytes.len());
    let cursor = Cursor::new(audio_bytes);
    let audio_reader = BufReader::new(cursor);

    // 7. Create an OutputStream and handle to control sinks
    //    This also initializes the default audio device via CPAL.
    let (_stream, stream_handle) =
        OutputStream::try_default().map_err(|e| format!("Failed to open audio output: {}", e))?;

    // 8. Create a Sink: this is your playback handle.
    let sink =
        Sink::try_new(&stream_handle).map_err(|e| format!("Failed to create sink: {}", e))?;

    // 9. Decode the OGG data into a Source of PCM samples.
    let source = Decoder::new(audio_reader).map_err(|e| format!("Failed to decode OGG: {}", e))?;

    // 10. Append the source to the sink; playback begins immediately in a background thread.
    sink.append(source);

    // 11. Block the main thread until the audio is done.
    sink.sleep_until_end();

    Ok(())
}

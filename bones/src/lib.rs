// SPDX-License-Identifier: Apache-2.0

use reqwest::Client;
use rodio::{Decoder, OutputStream, Sink};
use serde::Deserialize;
use serde_json::json;
use std::env;

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

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
async fn speak(msg: &str) -> Result<String, String> {
    // 1. API key
    let api_key =
        std::env::var("OPENAI_API_KEY").map_err(|e| format!("Missing OPENAI_API_KEY: {}", e))?;

    // 2. Build client + body
    let client = Client::new();
    let body = json!({
        "model": "gpt-4o-mini-tts",
        "voice": "alloy",
        "input": msg
    });

    // 3. Fire the request
    let resp = client
        .post("https://api.openai.com/v1/audio/speech")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Network error: {}", e))?;

    // 4. HTTP error check
    if !resp.status().is_success() {
        let err: ErrorResponse = resp
            .json()
            .await
            .map_err(|e| format!("Invalid error payload: {}", e))?;
        return Err(format!(
            "API error ({}): {}",
            err.error.code.unwrap_or_default(),
            err.error.message
        ));
    }

    // 5. Read bytes
    let audio_bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("Failed to read audio bytes: {}", e))?;

    // 6. Set up rodio
    let (_stream, handle) =
        OutputStream::try_default().map_err(|e| format!("Failed to open audio output: {}", e))?;
    let sink = Sink::try_new(&handle).map_err(|e| format!("Failed to create sink: {}", e))?;
    let cursor = std::io::Cursor::new(audio_bytes);
    let reader = std::io::BufReader::new(cursor);
    let source = Decoder::new(reader).map_err(|e| format!("Failed to decode OGG: {}", e))?;

    // 7. Play and wait
    sink.append(source);
    sink.sleep_until_end();

    Ok(format!("Can you hear it? \"{}\"", msg))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![speak])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

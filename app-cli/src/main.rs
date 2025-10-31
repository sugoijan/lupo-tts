// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use lupo_audio::Audio;
use lupo_tts_openai::OpenAi;

#[derive(Parser, Debug)]
#[command(author, version, about = "Simple TTS test harness", long_about = None)]
struct Cli {
    /// Text that should be synthesized and played
    #[arg(value_name = "MESSAGE")]
    message: Option<String>,

    /// TTS backend to use
    #[arg(long, value_enum, default_value_t = Backend::OpenAi)]
    backend: Backend,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum Backend {
    OpenAi,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Ignore the error if another subscriber already exists.
    let _ = tracing_subscriber::fmt::try_init();

    let cli = Cli::parse();
    let message = cli
        .message
        .unwrap_or_else(|| "Hello I'm ルポ-TTS.".to_string());

    match cli.backend {
        Backend::OpenAi => speak_with_openai(message).await?,
    }

    Ok(())
}

async fn speak_with_openai(message: String) -> Result<()> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .context("OPENAI_API_KEY environment variable is required")?;

    let tts = OpenAi::new(api_key);
    let audio_data = tts
        .text_to_speech(message)
        .await
        .context("failed to synthesize speech with OpenAI")?;

    let audio = Audio::start_default().context("failed to initialize audio output")?;

    audio
        .play_wav(audio_data)
        .await
        .context("failed to enqueue audio data for playback")?;

    audio
        .drain()
        .await
        .context("failed to wait for audio playback to finish")?;

    Ok(())
}

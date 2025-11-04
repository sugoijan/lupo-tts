use anyhow::{Context, Result, anyhow};
use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::generation::LogitsProcessor;
use candle_transformers::models::parler_tts::{Config, Model};
use std::sync::mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};
use tokenizers::Tokenizer;
use tracing::info;

#[derive(Clone, Debug)]
pub struct GenerationOptions {
    pub temperature: f64,
    pub top_p: Option<f64>,
    pub seed: u64,
    pub max_steps: usize,
}

impl Default for GenerationOptions {
    fn default() -> Self {
        Self {
            temperature: 0.0,
            top_p: None,
            seed: 0,
            max_steps: 500,
        }
    }
}

pub struct TtsModel {
    model: Model,
    device: Device,
    config: Config,
}

impl TtsModel {
    pub fn new(device: Device, config: Config, vb: VarBuilder<'_>) -> Result<Self> {
        let model = Model::new(&config, vb)?;
        Ok(Self {
            model,
            device,
            config,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.config.audio_encoder.sampling_rate
    }

    pub fn synthesize(
        &mut self,
        tokenizer: &Tokenizer,
        prompt: &str,
        description: &str,
        options: &GenerationOptions,
    ) -> Result<Vec<f32>> {
        let description_tokens = tokenizer
            .encode(description, true)
            .map_err(|err| anyhow!("failed to encode description prompt: {err}"))?
            .get_ids()
            .to_vec();
        let prompt_tokens = tokenizer
            .encode(prompt, true)
            .map_err(|err| anyhow!("failed to encode text prompt: {err}"))?
            .get_ids()
            .to_vec();

        let description_tokens = Tensor::new(description_tokens, &self.device)?
            .unsqueeze(0)
            .context("failed to add batch dimension to description tensor")?;
        let prompt_tokens = Tensor::new(prompt_tokens, &self.device)?
            .unsqueeze(0)
            .context("failed to add batch dimension to text tensor")?;

        let generation_start = Instant::now();
        info!(
            "starting Parler generation (max_steps={})",
            options.max_steps
        );
        let (progress_tx, progress_rx) = mpsc::channel();
        let progress_handle = std::thread::spawn(move || {
            let start = Instant::now();
            let mut ticks = 0usize;
            loop {
                match progress_rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        ticks += 1;
                        info!(
                            "generation still running… elapsed {:.1?} (~{} intervals)",
                            start.elapsed(),
                            ticks
                        );
                    }
                }
            }
        });

        let logits_processor =
            LogitsProcessor::new(options.seed, Some(options.temperature), options.top_p);
        let codes = self
            .model
            .generate(
                &prompt_tokens,
                &description_tokens,
                logits_processor,
                options.max_steps,
            )
            .context("failed to generate audio codes")?
            .to_dtype(DType::I64)
            .context("failed to cast audio codes to i64")?;
        let _ = progress_tx.send(());
        let _ = progress_handle.join();

        let codes = codes
            .unsqueeze(0)
            .context("failed to add batch dimension to audio codes")?;
        let (_, codebooks, frames) = codes
            .dims3()
            .context("unexpected audio code dimensions after generation")?;
        info!(
            "generation complete in {:.2?} → {} codebooks × {} frames",
            generation_start.elapsed(),
            codebooks,
            frames
        );

        let decode_start = Instant::now();
        info!("decoding audio codes into waveform…");
        let pcm = self
            .model
            .audio_encoder
            .decode_codes(&codes.to_device(&self.device)?)
            .context("failed to decode audio codes")?;

        let pcm = pcm
            .i((0, 0))
            .context("failed to select audio channel")?
            .to_vec1::<f32>()
            .context("failed to materialize audio")?;
        info!(
            "decoded waveform with {} samples in {:.2?}",
            pcm.len(),
            decode_start.elapsed()
        );

        Ok(pcm)
    }
}

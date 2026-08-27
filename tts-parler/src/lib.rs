mod model;

use anyhow::{Context, Result, anyhow};
use candle_core::{DType, Device};
use candle_nn::VarBuilder;
use hf_hub::progress::{DownloadEvent, ProgressEvent, ProgressHandler};
use hf_hub::repository::RepoTypeModel;
use hf_hub::{HFClientBuilder, HFRepositorySync, split_id};
use model::TtsModel;
use serde_json::Value;
use std::collections::HashSet;
use std::fs::File;
use std::io::Cursor;
use std::path::PathBuf;
use tokenizers::Tokenizer;

pub use model::GenerationOptions;

const DEFAULT_DESCRIPTION: &str = "A female speaker delivers a slightly expressive and animated speech with a moderate speed and pitch. The recording is of very high quality, with the speaker's voice sounding clear and very close up.";
const DEFAULT_REPO: &str = "parler-tts/parler-tts-mini-v1";

#[derive(Copy, Clone, Debug)]
pub enum DevicePreference {
    Auto,
    Cpu,
    Metal,
}

#[derive(Clone, Debug)]
pub struct ParlerLoadConfig {
    pub repo: String,
    pub revision: Option<String>,
    pub hf_token: Option<String>,
    pub default_description: String,
    pub device: DevicePreference,
    pub generation: GenerationOptions,
}

impl Default for ParlerLoadConfig {
    fn default() -> Self {
        Self {
            repo: DEFAULT_REPO.to_string(),
            revision: None,
            hf_token: None,
            default_description: DEFAULT_DESCRIPTION.to_string(),
            device: DevicePreference::Auto,
            generation: GenerationOptions::default(),
        }
    }
}

pub struct ParlerSynthesizer {
    tokenizer: Tokenizer,
    model: TtsModel,
    default_description: String,
    generation_defaults: GenerationOptions,
}

impl ParlerSynthesizer {
    pub fn new(config: ParlerLoadConfig) -> Result<Self> {
        let device = select_device(config.device)?;
        let assets = download_assets(
            &config.repo,
            config.revision.as_deref(),
            config.hf_token.as_deref(),
        )?;

        let tokenizer = Tokenizer::from_file(&assets.tokenizer)
            .map_err(|err| anyhow!("failed to load tokenizer json from huggingface: {err}"))?;

        let config_file = File::open(&assets.config)
            .with_context(|| format!("failed to open {}", assets.config.display()))?;
        let model_config: candle_transformers::models::parler_tts::Config =
            serde_json::from_reader(config_file).context("failed to parse config.json")?;

        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&assets.weights, DType::F32, &device)
                .context("failed to map model weights")?
        };
        let model = TtsModel::new(device, model_config, vb)?;

        Ok(Self {
            tokenizer,
            model,
            default_description: config.default_description,
            generation_defaults: config.generation,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.model.sample_rate()
    }

    pub fn default_generation(&self) -> &GenerationOptions {
        &self.generation_defaults
    }

    pub fn default_generation_mut(&mut self) -> &mut GenerationOptions {
        &mut self.generation_defaults
    }

    pub fn synthesize_waveform(
        &mut self,
        text: &str,
        description: Option<&str>,
        generation: Option<&GenerationOptions>,
    ) -> Result<Vec<f32>> {
        let description = description.unwrap_or(&self.default_description);
        let generation = generation.unwrap_or(&self.generation_defaults);
        self.model
            .synthesize(&self.tokenizer, text, description, generation)
    }

    pub fn synthesize_to_wav(
        &mut self,
        text: &str,
        description: Option<&str>,
        generation: Option<&GenerationOptions>,
    ) -> Result<Vec<u8>> {
        let waveform = self.synthesize_waveform(text, description, generation)?;
        waveform_to_wav(&waveform, self.sample_rate())
    }
}

struct ModelAssets {
    tokenizer: PathBuf,
    config: PathBuf,
    weights: Vec<PathBuf>,
}

/// Logs Hugging Face download lifecycle events through `tracing`.
struct TracingProgress;

impl ProgressHandler for TracingProgress {
    fn on_progress(&self, event: &ProgressEvent) {
        match event {
            ProgressEvent::Download(DownloadEvent::Start {
                total_files,
                total_bytes,
            }) => {
                tracing::info!(total_files, total_bytes, "downloading Hugging Face assets");
            }
            ProgressEvent::Download(DownloadEvent::Complete) => {
                tracing::info!("Hugging Face download complete");
            }
            _ => {}
        }
    }
}

struct RepoHandle {
    repo: HFRepositorySync<RepoTypeModel>,
    revision: Option<String>,
}

impl RepoHandle {
    fn fetch(&self, filename: &str) -> Result<PathBuf> {
        self.repo
            .download_file()
            .filename(filename)
            .maybe_revision(self.revision.clone())
            .progress(TracingProgress)
            .send()
            .with_context(|| format!("failed to fetch {filename} from huggingface"))
    }
}

fn download_assets(repo: &str, revision: Option<&str>, token: Option<&str>) -> Result<ModelAssets> {
    let mut client_builder = HFClientBuilder::new();
    if let Some(token) = token {
        client_builder = client_builder.token(token);
    }
    let client = client_builder
        .build_sync()
        .context("failed to build Hugging Face client")?;
    let (owner, name) = split_id(repo);
    let repo_handle = RepoHandle {
        repo: client.model(owner, name),
        revision: revision.map(str::to_owned),
    };

    let info = repo_handle
        .repo
        .info()
        .maybe_revision(repo_handle.revision.clone())
        .send()
        .context("failed to fetch Hugging Face repository metadata")?;
    let available_files: Vec<String> = info
        .siblings
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.rfilename)
        .collect();

    let tokenizer_name = find_tokenizer_file(&available_files).ok_or_else(|| {
        anyhow!(
            "tokenizer.json not found in repository. Available files: {}",
            describe_files(&available_files)
        )
    })?;
    let tokenizer = repo_handle.fetch(&tokenizer_name)?;

    let config_name = find_config_file(&available_files).ok_or_else(|| {
        anyhow!(
            "config.json not found in repository. Available files: {}",
            describe_files(&available_files)
        )
    })?;
    let config = repo_handle.fetch(&config_name)?;

    let weights = if let Some(index) = find_index_file(&available_files) {
        download_indexed_weights(&repo_handle, &index)?
    } else if let Some(weight_file) = find_weight_file(&available_files) {
        vec![repo_handle.fetch(&weight_file)?]
    } else {
        return Err(anyhow!(
            "model.safetensors not found in repository. Available files: {}",
            describe_files(&available_files)
        ));
    };

    Ok(ModelAssets {
        tokenizer,
        config,
        weights,
    })
}

/// The Hub API may omit the file listing entirely (`siblings: null`); say so
/// instead of printing a blank list that reads like an empty repository.
fn describe_files(files: &[String]) -> String {
    if files.is_empty() {
        "(the repository file listing was empty or unavailable)".to_string()
    } else {
        files.join(", ")
    }
}

fn find_tokenizer_file(files: &[String]) -> Option<String> {
    pick_file(files, "tokenizer.json").or_else(|| {
        files
            .iter()
            .find(|entry| entry.ends_with("tokenizer.json"))
            .cloned()
    })
}

fn find_config_file(files: &[String]) -> Option<String> {
    pick_file(files, "config.json").or_else(|| {
        files
            .iter()
            .find(|entry| entry.ends_with("config.json"))
            .cloned()
    })
}

fn find_weight_file(files: &[String]) -> Option<String> {
    pick_file(files, "model.safetensors").or_else(|| {
        files
            .iter()
            .find(|entry| entry.ends_with("model.safetensors"))
            .cloned()
    })
}

fn find_index_file(files: &[String]) -> Option<String> {
    pick_file(files, "model.safetensors.index.json").or_else(|| {
        files
            .iter()
            .find(|entry| entry.ends_with("model.safetensors.index.json"))
            .cloned()
    })
}

fn pick_file(files: &[String], filename: &str) -> Option<String> {
    files
        .iter()
        .find(|entry| entry.as_str() == filename)
        .cloned()
        .or_else(|| {
            let candidate = format!("/{filename}");
            files
                .iter()
                .find(|entry| entry.ends_with(&candidate))
                .cloned()
        })
}

fn download_indexed_weights(repo_handle: &RepoHandle, index: &str) -> Result<Vec<PathBuf>> {
    let index_path = repo_handle.fetch(index)?;
    let file = File::open(&index_path)
        .with_context(|| format!("failed to open {}", index_path.display()))?;
    let index_json: Value =
        serde_json::from_reader(file).with_context(|| format!("failed to parse {index}"))?;
    let weight_map = index_json
        .get("weight_map")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("weight map is missing from {index}"))?;
    let mut shards = HashSet::new();
    for value in weight_map.values() {
        if let Some(filename) = value.as_str() {
            shards.insert(filename.to_string());
        }
    }
    if shards.is_empty() {
        return Err(anyhow!(
            "weight map in {index} does not reference any files"
        ));
    }
    let mut result = Vec::new();
    for shard in shards {
        result.push(repo_handle.fetch(&shard)?);
    }
    Ok(result)
}

fn waveform_to_wav(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec)?;
        for sample in samples {
            let sample = sample.clamp(-1.0, 1.0);
            let quantized = (sample * i16::MAX as f32) as i16;
            writer.write_sample(quantized)?;
        }
        writer.finalize()?;
    }
    Ok(cursor.into_inner())
}

fn select_device(preference: DevicePreference) -> Result<Device> {
    match preference {
        DevicePreference::Cpu => Ok(Device::Cpu),
        DevicePreference::Auto => {
            #[cfg(target_os = "macos")]
            {
                match Device::new_metal(0) {
                    Ok(device) => return Ok(device),
                    Err(err) => {
                        tracing::warn!("Metal backend unavailable ({err}), falling back to CPU");
                    }
                }
            }
            Ok(Device::Cpu)
        }
        DevicePreference::Metal => {
            #[cfg(target_os = "macos")]
            {
                Device::new_metal(0).context("failed to initialize Metal (MPS) device")
            }
            #[cfg(not(target_os = "macos"))]
            {
                Err(anyhow!("Metal backend is only available on macOS"))
            }
        }
    }
}

mod model;

use anyhow::{Context, Result, anyhow};
use candle_core::{DType, Device};
use candle_nn::VarBuilder;
use hf_hub::api::sync::ApiBuilder;
use hf_hub::{Repo, RepoType};
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

fn download_assets(repo: &str, revision: Option<&str>, token: Option<&str>) -> Result<ModelAssets> {
    let mut api_builder = ApiBuilder::new().with_progress(true);
    if let Some(token) = token {
        api_builder = api_builder.with_token(Some(token.to_owned()));
    }
    let api = api_builder.build()?;
    let repo = if let Some(revision) = revision {
        Repo::with_revision(repo.to_string(), RepoType::Model, revision.to_string())
    } else {
        Repo::new(repo.to_string(), RepoType::Model)
    };
    let repo_handle = api.repo(repo);

    let info = repo_handle
        .info()
        .context("failed to fetch Hugging Face repository metadata")?;
    let available_files: Vec<String> = info.siblings.into_iter().map(|s| s.rfilename).collect();

    let tokenizer_name = find_tokenizer_file(&available_files).ok_or_else(|| {
        anyhow!(
            "tokenizer.json not found in repository. Available files: {}",
            available_files.join(", ")
        )
    })?;
    let tokenizer = repo_handle
        .get(&tokenizer_name)
        .with_context(|| format!("failed to fetch {tokenizer_name} from huggingface"))?;

    let config_name = find_config_file(&available_files).ok_or_else(|| {
        anyhow!(
            "config.json not found in repository. Available files: {}",
            available_files.join(", ")
        )
    })?;
    let config = repo_handle
        .get(&config_name)
        .with_context(|| format!("failed to fetch {config_name} from huggingface"))?;

    let weights = if let Some(index) = find_index_file(&available_files) {
        download_indexed_weights(&repo_handle, &index)?
    } else if let Some(weight_file) = find_weight_file(&available_files) {
        vec![
            repo_handle
                .get(&weight_file)
                .with_context(|| format!("failed to fetch {weight_file} from huggingface"))?,
        ]
    } else {
        return Err(anyhow!(
            "model.safetensors not found in repository. Available files: {}",
            available_files.join(", ")
        ));
    };

    Ok(ModelAssets {
        tokenizer,
        config,
        weights,
    })
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

fn download_indexed_weights(
    repo_handle: &hf_hub::api::sync::ApiRepo,
    index: &str,
) -> Result<Vec<PathBuf>> {
    let index_path = repo_handle
        .get(index)
        .with_context(|| format!("failed to fetch {index} from huggingface"))?;
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
        let path = repo_handle
            .get(&shard)
            .with_context(|| format!("failed to fetch {shard} from huggingface"))?;
        result.push(path);
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

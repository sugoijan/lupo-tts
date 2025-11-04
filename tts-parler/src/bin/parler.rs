use anyhow::{Context, Result};
use clap::Parser;
use lupo_tts_parler::{DevicePreference, ParlerLoadConfig, ParlerSynthesizer};

#[derive(Parser, Debug)]
struct Cli {
    /// Text that should be synthesized
    #[arg(value_name = "TEXT")]
    text: String,

    /// Output WAV path
    #[arg(short, long, default_value = "output.wav")]
    output: std::path::PathBuf,

    /// Hugging Face repository slug (e.g. metavoiceio/metavoice-1B-whisper-en)
    #[arg(long, default_value = "parler-tts/parler-tts-mini-v1")]
    repo: String,

    /// Optional revision (tag/commit) within the repository
    #[arg(long)]
    revision: Option<String>,

    /// Hugging Face access token (falls back to HF_TOKEN environment variable)
    #[arg(long, env = "HF_TOKEN")]
    hf_token: Option<String>,

    /// Description prompt that controls speaker identity and style
    #[arg(
        long,
        default_value = "A female speaker delivers a slightly expressive and animated speech with a moderate speed and pitch. The recording is of very high quality, with the speaker's voice sounding clear and very close up."
    )]
    description: String,

    /// The temperature used to generate samples.
    #[arg(long, default_value_t = 0.0)]
    temperature: f64,

    /// Nucleus sampling probability cutoff.
    #[arg(long)]
    top_p: Option<f64>,

    /// Random seed used for generation.
    #[arg(long, default_value_t = 0)]
    seed: u64,

    /// Maximum number of decoding steps.
    #[arg(long, default_value_t = 500)]
    max_steps: usize,

    /// Force the computation to run on CPU even if GPU support is available.
    #[arg(long)]
    cpu: bool,

    /// Attempt to run on Apple's Metal (MPS) backend.
    #[cfg(target_os = "macos")]
    #[arg(long)]
    metal: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    let args = Cli::parse();
    let mut load_config = ParlerLoadConfig::default();
    let device_pref = select_device_preference(&args);
    load_config.repo = args.repo.clone();
    load_config.revision = args.revision.clone();
    load_config.hf_token = args.hf_token.clone();
    load_config.default_description = args.description.clone();
    load_config.device = device_pref;

    let mut synthesizer = ParlerSynthesizer::new(load_config)?;
    let mut generation = synthesizer.default_generation().clone();
    generation.temperature = args.temperature;
    generation.top_p = args.top_p;
    generation.seed = args.seed;
    generation.max_steps = args.max_steps;

    let wav_data = synthesizer.synthesize_to_wav(&args.text, None, Some(&generation))?;
    std::fs::write(&args.output, wav_data)
        .with_context(|| format!("failed to write {}", args.output.display()))?;

    tracing::info!("wrote {}", args.output.display());
    Ok(())
}

fn select_device_preference(args: &Cli) -> DevicePreference {
    if args.cpu {
        DevicePreference::Cpu
    } else {
        #[cfg(target_os = "macos")]
        {
            if args.metal {
                return DevicePreference::Metal;
            }
        }
        DevicePreference::Auto
    }
}

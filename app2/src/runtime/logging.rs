// SPDX-License-Identifier: Apache-2.0

use std::error::Error;
use std::path::Path;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_error::ErrorLayer;
use tracing_subscriber::{
    filter::{EnvFilter, LevelFilter},
    fmt,
    prelude::*,
};

#[allow(dead_code)]
pub struct LogGuard(WorkerGuard);

pub fn setup_logging(log_dir: impl AsRef<Path>) -> Result<LogGuard, Box<dyn Error>> {
    // Use app2-specific file naming so trial logs are separated from the GPUI shell.
    let (writer, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily(log_dir, "lupo2-log"));

    // Build an EnvFilter (fallback to INFO if LUPO_LOG unset)
    let env_filter_builder = EnvFilter::builder()
        .with_env_var("LUPO_LOG")
        .with_default_directive(LevelFilter::INFO.into());
    let env_filter_builder_file = env_filter_builder.clone();
    let env_filter_file = env_filter_builder_file.from_env_lossy();

    // Compose your layers
    let json_format = fmt::format()
        .json() // full JSON format
        .flatten_event(true) // no { "fields": { … } } wrapper
        .with_file(false) // drop any "file" metadata
        .with_line_number(false) // drop any "line" metadata
        .with_target(true); // keep the "target" key

    let file_layer = fmt::layer()
        .json()
        .event_format(json_format)
        .with_writer(writer)
        .with_filter(env_filter_file)
        .boxed();

    #[cfg(debug_assertions)]
    {
        let env_filter = env_filter_builder.from_env_lossy();
        let stdout_layer = fmt::layer().compact().with_filter(env_filter).boxed();

        tracing_subscriber::registry()
            .with(ErrorLayer::default())
            .with(file_layer)
            .with(stdout_layer)
            .init();
        tracing::info!(
            "logging initialized (debug build): writing JSON logs to file and compact logs to stdout"
        );
    }

    #[cfg(not(debug_assertions))]
    {
        tracing_subscriber::registry()
            .with(ErrorLayer::default())
            .with(file_layer)
            .init();
    }

    Ok(LogGuard(guard))
}

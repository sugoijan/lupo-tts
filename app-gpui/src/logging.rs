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
    // 2. Create a non-blocking, daily-rotated file writer _and_ its guard
    let (writer, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily(log_dir, "log"));

    // 3. Build an EnvFilter (fallback to INFO if LUPO_LOG unset)
    let env_filter_builder = EnvFilter::builder()
        .with_env_var("LUPO_LOG")
        .with_default_directive(LevelFilter::INFO.into());
    let env_filter_builder_file = env_filter_builder.clone();
    let env_filter = env_filter_builder.from_env_lossy();
    let env_filter_file = env_filter_builder_file.from_env_lossy();

    // 4. Compose your layers
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

    let stdout_layer = fmt::layer().compact().with_filter(env_filter).boxed();

    // 5. Register them all—in this example with layer-level filtering:
    tracing_subscriber::registry()
        .with(ErrorLayer::default())
        .with(file_layer)
        .with(stdout_layer)
        //.with(console_subscriber::ConsoleLayer::builder().spawn()) // your tokio-console
        .init();

    Ok(LogGuard(guard))
}

#[cfg(not(target_os = "macos"))]
compile_error!("lupo-tts-macos is only supported on macOS");

// SPDX-License-Identifier: Apache-2.0

mod manager;
mod protocol;

pub use manager::{
    HELPER_BINARY_NAME, HELPER_PATH_ENV, SystemSpeechProcessConfig, SystemSpeechProcessError,
    SystemSpeechProcessManager,
};

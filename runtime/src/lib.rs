// SPDX-License-Identifier: Apache-2.0

//! Frontend-agnostic backend for LUPO TTS: chat ingestion, the message
//! pipeline, logging, and the event bridge that frontends poll.

pub mod bridge;
pub mod logging;
pub mod pipeline;
pub mod services;
pub mod twitch;
pub mod ui_protocol;
pub mod youtube;

pub use services::AppServices;

/// Install the process-level rustls crypto provider.
///
/// The dependency graph compiles rustls with both the `ring` and `aws-lc-rs`
/// backends, so rustls cannot pick one automatically; building any TLS config
/// (websockets, HTTP clients) would panic without an explicit choice.
/// Idempotent: a repeated call is a no-op. Every frontend must call this
/// before starting the runtime.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

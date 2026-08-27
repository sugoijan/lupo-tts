// SPDX-License-Identifier: Apache-2.0

/// Guards against the dependency graph enabling more than one rustls crypto
/// backend without the app selecting one: `ClientConfig::builder()` panics in
/// that situation unless a provider was installed first.
#[test]
fn rustls_client_config_builder_is_available_after_install() {
    lupo_runtime::install_crypto_provider();
    let _ = rustls::ClientConfig::builder();
}

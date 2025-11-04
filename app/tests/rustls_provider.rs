#[test]
fn rustls_client_config_builder_is_available() {
    let _ = rustls::ClientConfig::builder();
}

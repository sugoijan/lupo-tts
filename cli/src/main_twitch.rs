// SPDX-License-Identifier: Apache-2.0

// twitch_pkce_cli.rs

// Cargo.toml dependencies:
// [dependencies]
// tokio = { version = "1.28", features = ["full"] }
// oauth2 = { version = "5.0", features = ["reqwest", "pkce"] }
// opener = "0.6"
// url = "2.4"
// dotenvy = "0.15"

use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl,
    AuthorizationCode,
    ClientId,
    CsrfToken,
    PkceCodeChallenge,
    PkceCodeVerifier,
    RedirectUrl,
    Scope,
    TokenUrl,
};
use oauth2::reqwest::async_http_client;
use std::{error::Error, io::{self, Write}};
use url::Url;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Load environment variables:
    // TWITCH_CLIENT_ID, TWITCH_REDIRECT_URI
    dotenvy::dotenv().ok();
    let client_id = std::env::var("TWITCH_CLIENT_ID")?;
    let redirect_uri = std::env::var("TWITCH_REDIRECT_URI")?; // e.g. http://localhost:8080/callback or custom scheme

    // OAuth2 endpoints
    let auth_url = AuthUrl::new("https://id.twitch.tv/oauth2/authorize".to_string())?;
    let token_url = TokenUrl::new("https://id.twitch.tv/oauth2/token".to_string())?;

    // Build the OAuth2 client without a client secret, using PKCE
    let client = BasicClient::new(
        ClientId::new(client_id),
        None,
        auth_url,
        Some(token_url),
    )
    .set_redirect_uri(RedirectUrl::new(redirect_uri.clone())?);

    // Generate PKCE code verifier and challenge
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    // Generate the authorization URL
    let (auth_url, _csrf_state) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new("chat:read".to_string()))
        .add_scope(Scope::new("chat:edit".to_string()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    println!("Open this URL in your browser (or paste it):\n{}\n", auth_url);
    // Try to launch automatically (may fail silently)
    let _ = opener::open(auth_url.as_str());

    // Read the redirected URL from stdin
    print!("After authorization, paste the full redirect URL here: ");
    io::stdout().flush()?;
    let mut redirect_input = String::new();
    io::stdin().read_line(&mut redirect_input)?;
    let redirect_url = Url::parse(redirect_input.trim())?;

    // Extract the authorization code
    let code_pair = redirect_url
        .query_pairs()
        .find(|(key, _)| key == "code")
        .ok_or("Missing `code` parameter in redirect URL")?;
    let code = AuthorizationCode::new(code_pair.1.into_owned());

    // Exchange the code for a token, using our PKCE verifier
    let token_response = client
        .exchange_code(code)
        .set_pkce_verifier(pkce_verifier)
        .request_async(async_http_client)
        .await?;

    println!("\nAccess token: {}", token_response.access_token().secret());
    if let Some(refresh) = token_response.refresh_token() {
        println!("Refresh token: {}", refresh.secret());
    }

    Ok(())
}

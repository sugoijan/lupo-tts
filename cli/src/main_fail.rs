// SPDX-License-Identifier: Apache-2.0

use reqwest::Client;
use serde::Deserialize;
use anyhow::Result;

#[derive(Debug, Deserialize)]
struct HelixChatters {
    data: Vec<serde_json::Value>,
    pagination: serde_json::Value,
}

async fn fetch_chatters_helix(
    client_id: &str,
    oauth_token: &str,
    broadcaster_id: &str,
) -> Result<HelixChatters> {
    let client = Client::builder()
        .user_agent("lupo-tts/0.1")
        .build()?;

    let url = format!(
        "https://api.twitch.tv/helix/chat/chatters?broadcaster_id={}",
        broadcaster_id
    );

    let resp = client
        .get(&url)
        .header("Client-ID", client_id)
        .bearer_auth(oauth_token)
        .send()
        .await?
        .error_for_status()?;   // will 404 if broadcaster_id is invalid

    let result = resp.json::<HelixChatters>().await?;
    Ok(result)
}


#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let client_id = "r516rs332zdfv6bxotzr9bgvszl6v1";
    let oauth = std::env::var("TWITCH_OAUTH_TOKEN")?;
    let channel = "sugoijan";

    let chatters = fetch_chatters_helix(client_id, &oauth, channel).await?;
    println!("Current chatters: {:#?}", chatters.data);

    Ok(())
}

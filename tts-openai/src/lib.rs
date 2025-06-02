// SPDX-License-Identifier: Apache-2.0

use bytes::Bytes;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use tracing::*;

static OPENAI_API_V1_SPEECH: LazyLock<Url> =
    LazyLock::new(|| Url::parse("https://api.openai.com/v1/audio/speech").unwrap());

// create the error type that represents all errors possible in our program
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("error during API request")]
    Request(#[from] reqwest::Error),
    #[error("API error")]
    Api(#[from] ApiError),
}

// we must manually implement serde::Serialize
impl Serialize for Error {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        serializer.serialize_str(self.to_string().as_ref())
    }
}

#[derive(Debug, Serialize)]
struct SpeechRequest {
    model: String,
    voice: String,
    input: String,
    response_format: String,
}

impl SpeechRequest {
    fn new_default<S: Into<String>>(input: S) -> Self {
        Self {
            model: "gpt-4o-mini-tts".to_string(),
            voice: "alloy".to_string(),
            input: input.into(),
            response_format: "wav".to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, thiserror::Error)]
#[error("OpenAI API error: {message}")]
pub struct ApiError {
    message: String,
    #[serde(default)]
    code: Option<String>,
}

#[derive(Deserialize)]
struct SpeechErrResponse {
    error: ApiError,
}

#[derive(Debug)]
pub struct OpenAi {
    client: Client,
    api_key: String,
}

impl Default for OpenAi {
    fn default() -> Self {
        Self {
            client: Client::new(),
            api_key: String::new(),
        }
    }
}

impl OpenAi {
    pub fn new<S: Into<String>>(api_key: S) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
        }
    }

    pub async fn text_to_speech<S: Into<String>>(&self, message: S) -> Result<Bytes, Error> {
        let body = SpeechRequest::new_default(message);

        debug!("send request to OpenAI: {:?}", &body);
        let resp = self
            .client
            .post(OPENAI_API_V1_SPEECH.clone())
            .bearer_auth(self.api_key.clone())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err: SpeechErrResponse = resp.json().await?;
            debug!("not sucessful: {:?}", &err.error);
            return Err(err.error.into());
        }

        Ok(resp.bytes().await?)
    }
}

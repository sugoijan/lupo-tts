// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestEnvelope {
    Hello { protocol_version: u32 },
    Synthesize { request_id: u64, text: String },
    Shutdown { request_id: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseEnvelope {
    HelloOk {
        protocol_version: u32,
    },
    SynthesizeOk {
        request_id: u64,
        wav_base64: String,
    },
    ShutdownOk {
        request_id: u64,
    },
    Error {
        request_id: Option<u64>,
        code: String,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        let request = RequestEnvelope::Synthesize {
            request_id: 42,
            text: "hello".to_string(),
        };

        let json = serde_json::to_string(&request).expect("request should serialize");
        let decoded: RequestEnvelope =
            serde_json::from_str(&json).expect("request should deserialize");

        match decoded {
            RequestEnvelope::Synthesize { request_id, text } => {
                assert_eq!(request_id, 42);
                assert_eq!(text, "hello");
            }
            other => panic!("unexpected request variant: {other:?}"),
        }
    }

    #[test]
    fn response_round_trip() {
        let response = ResponseEnvelope::SynthesizeOk {
            request_id: 7,
            wav_base64: "Zm9v".to_string(),
        };

        let json = serde_json::to_string(&response).expect("response should serialize");
        let decoded: ResponseEnvelope =
            serde_json::from_str(&json).expect("response should deserialize");

        match decoded {
            ResponseEnvelope::SynthesizeOk {
                request_id,
                wav_base64,
            } => {
                assert_eq!(request_id, 7);
                assert_eq!(wav_base64, "Zm9v");
            }
            other => panic!("unexpected response variant: {other:?}"),
        }
    }
}

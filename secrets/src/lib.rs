// SPDX-License-Identifier: Apache-2.0

use serde::de::value::StrDeserializer;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::slice::Iter;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum Secret {
    OpenaiApiKey,
    TwitchAccessToken,
    TwitchRefreshToken,
}

pub mod secure;
pub use secure::{
    SecureStoreError, SecureStoreResult, delete_secret, load_secret_string, store_secret,
};

impl Secret {
    pub fn iter_all_secrets() -> Iter<'static, Self> {
        static ALL: [Secret; 3] = [
            Secret::OpenaiApiKey,
            Secret::TwitchAccessToken,
            Secret::TwitchRefreshToken,
        ];
        ALL.iter()
    }

    pub fn override_from_env(self) -> bool {
        match self {
            Secret::OpenaiApiKey => false,
            Secret::TwitchAccessToken => false,
            Secret::TwitchRefreshToken => false,
        }
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.serialize(f).map_err(|_| fmt::Error)
    }
}

impl FromStr for Secret {
    type Err = serde::de::value::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let de = StrDeserializer::new(s);
        Self::deserialize(de)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display() {
        // When you `format!` a Secret::OpenaiApiKey, it should serialize to "OPENAI_API_KEY"
        let secret = Secret::OpenaiApiKey;
        let s = format!("{}", secret);
        assert_eq!(s, "OPENAI_API_KEY");
    }

    #[test]
    fn test_from_str_valid() {
        // Parsing the exact SCREAMING_SNAKE_CASE name should succeed
        let parsed = Secret::from_str("OPENAI_API_KEY").unwrap();
        assert_eq!(parsed, Secret::OpenaiApiKey);
    }

    #[test]
    fn test_from_str_invalid() {
        // Any other string should return an Err
        assert!(Secret::from_str("openai_api_key").is_err());
        assert!(Secret::from_str("INVALID_VARIANT").is_err());
        assert!(Secret::from_str("").is_err());
    }
}

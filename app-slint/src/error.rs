// SPDX-License-Identifier: Apache-2.0

// create the error type that represents all errors possible in our program
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("could not parse environment variable")]
    EnvVar(#[from] std::env::VarError),
    #[error("OpenAI API returned error")]
    OpenAi(#[from] lupo_tts_openai::Error),
    #[error("error while playing audio")]
    Audio(#[from] lupo_audio::Error),
    #[error("database error")]
    DbError(#[from] sea_orm::DbErr),
}

// we must manually implement serde::Serialize
impl serde::Serialize for Error {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        serializer.serialize_str(self.to_string().as_ref())
    }
}

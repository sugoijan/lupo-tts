// SPDX-License-Identifier: Apache-2.0

use tracing::*;

mod audio;
mod commands;
mod db;
mod logging;

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
    #[error("Tauri error")]
    TauriError(#[from] tauri::Error),
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

pub async fn run() {
    let mut builder = tauri::Builder::default();

    // add optional single-instance plugin, it MUST be the first plugin
    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|_app, argv, _cwd| {
          println!("a new app instance was opened with {argv:?} and the deep link event was already triggered");
          // when defining deep link schemes at runtime, you must also check `argv` here
        }));
    }

    // add plugins
    builder = builder.invoke_handler(tauri::generate_handler![
        commands::play,
        commands::list_messages,
        commands::queue_message,
    ]);

    // build app
    let mut app = builder
        .build(tauri::generate_context!())
        .expect("error while running tauri application");

    logging::setup_logging(&mut app).expect("error while setting up logging");

    db::setup_db(&mut app)
        .await
        .expect("error while setting up db");

    audio::setup_audio(&mut app).expect("error while setting up audio");

    // run app, this never returns
    info!("start app");
    app.run(|_, _event| {});

    unreachable!();
}

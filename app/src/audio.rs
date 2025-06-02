// SPDX-License-Identifier: Apache-2.0

use tauri::Manager;
use tracing::*;

#[instrument(skip(app))]
pub fn setup_audio(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let audio = lupo_audio::Audio::start_default()?;
    app.manage(audio);
    Ok(())
}

// SPDX-License-Identifier: Apache-2.0

use tracing::*;

//mod commands;
mod db;
mod error;
mod logging;
mod state;
mod ui;

#[cfg_attr(target_arch = "wasm32", wasm_bindgen(start))]
pub fn main() {
    use slint::ComponentHandle;

    let state = state::State::new();

    info!("start app");
    let main_window = state.main_window.clone_strong();

    main_window.run().unwrap();
}

// SPDX-License-Identifier: Apache-2.0

mod app;
pub(crate) mod commands;

use app::*;
use leptos::prelude::*;
use tracing::*;

fn main() {
    console_error_panic_hook::set_once();

    tracing_wasm::try_set_as_global_default().expect("error while setting up tracing");

    info!("start app");
    mount_to_body(|| {
        view! {
            <App/>
        }
    })
}

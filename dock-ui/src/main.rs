// SPDX-License-Identifier: Apache-2.0

//! The LUPO dock UI: a Dioxus app compiled to wasm, served by the plugin's
//! local server (see `obs/src/dock.rs`) and rendered inside an OBS browser
//! dock. It speaks `lupo-ui-protocol` over the token-protected WebSocket; the
//! same page would work unchanged in any webview shell.
//!
//! Build with `cargo xtask build-dock-ui` (wasm + wasm-bindgen); the native
//! build of this crate is an empty stub so `cargo build --workspace` stays
//! green on non-wasm targets.

#[cfg(target_arch = "wasm32")]
mod app;

fn main() {
    #[cfg(target_arch = "wasm32")]
    dioxus::launch(app::App);
}

// SPDX-License-Identifier: Apache-2.0

mod action;
mod model;
mod renderer;
mod state;
mod ui;

#[cfg(all(feature = "renderer-webview", feature = "renderer-native"))]
compile_error!("Enable exactly one renderer feature: renderer-native or renderer-webview.");

#[cfg(not(any(feature = "renderer-webview", feature = "renderer-native")))]
compile_error!("Enable one renderer feature: renderer-native or renderer-webview.");

pub fn run() {
    renderer::run();
}

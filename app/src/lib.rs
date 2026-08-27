// SPDX-License-Identifier: Apache-2.0

mod app;

pub fn run() {
    lupo_runtime::install_crypto_provider();
    app::run();
}

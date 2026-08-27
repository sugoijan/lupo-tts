// SPDX-License-Identifier: Apache-2.0

//! Raw FFI bindings to libobs.
//!
//! The bindings are generated from `vendor/obs-studio/libobs` (submodule pinned to
//! the OBS release we target) by `cargo xtask obs-bindgen` and committed per target
//! triple under `src/bindings/`. Regenerate after bumping the submodule.
//!
//! Symbols are resolved against the host OBS process at load time; nothing links
//! against libobs at build time (see `obs/build.rs`), so this crate builds on
//! machines without OBS installed.
//!
//! Module entry points that a plugin *defines* (`obs_module_load`, `obs_module_ver`,
//! `obs_current_module`, ...) are deliberately excluded from the bindings; the
//! plugin crate exports them.

#![allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    unsafe_op_in_unsafe_fn,
    clippy::all
)]

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
include!("bindings/aarch64-apple-darwin.rs");

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!(
    "no libobs bindings for this target yet; run `cargo xtask obs-bindgen` on it and add an include! above"
);

/// `MAKE_SEMANTIC_VERSION` from `obs-config.h` (a function-like macro, so bindgen
/// cannot generate it).
pub const fn make_semantic_version(major: u32, minor: u32, patch: u32) -> u32 {
    (major << 24) | (minor << 16) | patch
}

/// `LIBOBS_API_VER`: the libobs version the bindings were generated from. A module
/// reports this from `obs_module_ver`.
pub const LIBOBS_API_VER: u32 = make_semantic_version(
    LIBOBS_API_MAJOR_VER,
    LIBOBS_API_MINOR_VER,
    LIBOBS_API_PATCH_VER,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_version_packs_like_the_c_macro() {
        assert_eq!(make_semantic_version(31, 0, 3), 0x1F00_0003);
        assert_eq!(LIBOBS_API_VER >> 24, LIBOBS_API_MAJOR_VER);
    }
}

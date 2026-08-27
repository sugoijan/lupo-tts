// SPDX-License-Identifier: Apache-2.0

//! libobs symbols are provided by the OBS process that loads the plugin, so the
//! cdylib is linked with unresolved symbols allowed instead of against a libobs
//! import library. This keeps the build independent of an OBS installation.
//! Windows needs an import library instead (`obs.lib`); see OBS_PLUGIN_EXPLORATION.md §8.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=csrc/dock_shim.cpp");
    // The only C++ in the plugin: obs-browser's panel API for a native dock.
    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .file("csrc/dock_shim.cpp")
        .compile("lupo_dock_shim");
    if cfg!(target_os = "macos") {
        // Applies to the cdylib and to this crate's test binaries (which must
        // not actually call into libobs).
        println!("cargo:rustc-link-arg=-undefined");
        println!("cargo:rustc-link-arg=dynamic_lookup");
    }
}

// SPDX-License-Identifier: Apache-2.0

//! LUPO TTS as an OBS Studio plugin.
//!
//! This crate is a `cdylib` that OBS loads as a module. It exports the C entry
//! points OBS expects (`obs_module_load` & co., normally produced by the
//! `OBS_DECLARE_MODULE()` macro) and, later, registers the `lupo_tts` audio
//! source. Everything crossing the FFI boundary is wrapped in [`ffi_guard`] so a
//! panic is logged instead of unwinding into libobs.

mod audio;
mod chat;
mod dock;
mod engine;
mod log;
mod sink;
mod source;

use std::ffi::c_char;
use std::panic::{AssertUnwindSafe, UnwindSafe, catch_unwind};
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use lupo_obs_sys as sys;
use tracing::{error, info};

const MODULE_NAME: &std::ffi::CStr = c"LUPO TTS";
const MODULE_DESCRIPTION: &std::ffi::CStr =
    c"Reads live chat messages out loud as an audio source.";

static MODULE_POINTER: AtomicPtr<sys::obs_module_t> = AtomicPtr::new(ptr::null_mut());

/// Run `f`, converting a panic into a log line and `fallback` instead of an
/// unwind across the C boundary (which would abort the OBS process).
pub(crate) fn ffi_guard<T>(
    what: &'static str,
    fallback: T,
    f: impl FnOnce() -> T + UnwindSafe,
) -> T {
    match catch_unwind(f) {
        Ok(value) => value,
        Err(payload) => {
            let detail = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".to_string());
            // The log layer may itself be what panicked; fall back to blog directly.
            let message = format!("panic in {what}: {detail}");
            log::blog_raw(sys::LOG_ERROR, &message);
            error!(what, detail, "panic caught at FFI boundary");
            fallback
        }
    }
}

// --- Entry points OBS resolves with dlsym (see libobs/obs-module.c) -----------

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_set_pointer(module: *mut sys::obs_module_t) {
    MODULE_POINTER.store(module, Ordering::SeqCst);
}

/// Declared in `obs-module.h` for libobs helpers (`obs_module_text`, config
/// paths) but defined by each module.
#[unsafe(no_mangle)]
pub extern "C" fn obs_current_module() -> *mut sys::obs_module_t {
    MODULE_POINTER.load(Ordering::SeqCst)
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_ver() -> u32 {
    sys::LIBOBS_API_VER
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_name() -> *const c_char {
    MODULE_NAME.as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_description() -> *const c_char {
    MODULE_DESCRIPTION.as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_load() -> bool {
    ffi_guard("obs_module_load", false, || {
        log::install();
        info!(
            version = env!("CARGO_PKG_VERSION"),
            libobs_api = format_args!(
                "{}.{}.{}",
                sys::LIBOBS_API_MAJOR_VER,
                sys::LIBOBS_API_MINOR_VER,
                sys::LIBOBS_API_PATCH_VER
            ),
            "lupo module loaded"
        );
        source::register();
        true
    })
}

/// Runs after every module is loaded and before the scene collection is
/// restored, so sources created during collection load find the engine.
#[unsafe(no_mangle)]
pub extern "C" fn obs_module_post_load() {
    ffi_guard("obs_module_post_load", (), || {
        let started = std::time::Instant::now();
        match engine::start_for_module() {
            Ok(()) => info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "engine started"
            ),
            Err(err) => {
                error!(%err, "engine failed to start; sources will only offer test playback")
            }
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_unload() {
    ffi_guard(
        "obs_module_unload",
        (),
        AssertUnwindSafe(|| {
            info!("lupo module unloading");
            let started = std::time::Instant::now();
            engine::shutdown();
            info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "engine stopped"
            );
        }),
    )
}

// SPDX-License-Identifier: Apache-2.0

//! The `lupo_tts` OBS source: an audio-only source whose samples come from the
//! LUPO pipeline hosted by [`crate::engine`]. Operation and settings live in
//! the dock (see [`crate::dock`]); Properties only carry what is genuinely
//! per-source: the "global output" toggle, status, and debug tools.
//!
//! Output modes per instance:
//! - **scene-bound** (default): the source is heard wherever it is placed and
//!   goes silent when the program scene does not contain it;
//! - **global** (`global` setting): the instance is also claimed into a free
//!   OBS output channel (`obs_set_output_source`), the mechanism behind
//!   Settings → Audio global devices, so it stays audible in every scene.

use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lupo_obs_sys as sys;
use tracing::{info, warn};

use crate::audio::{Player, SourceHandle, tone};
use crate::sink::TargetId;
use crate::{engine, ffi_guard};

const SOURCE_ID: &CStr = c"lupo_tts";
const SOURCE_NAME: &CStr = c"LUPO TTS";

const SETTING_GLOBAL: &CStr = c"global";
const SETTING_ENGINE_STATUS: &CStr = c"engine_status";
const SETTING_DOCK_URL: &CStr = c"dock_url";
const SETTING_DEBUG_TOOLS: &CStr = c"debug_tools";

/// `MAX_CHANNELS` from `obs-defs.h`. Channel 0 is the program scene, 1–5 are
/// OBS's global audio devices; we scan downwards from the top to stay clear of
/// other plugins that count upwards.
const OBS_MAX_CHANNELS: u32 = 64;
const FIRST_CLAIMABLE_CHANNEL: u32 = 6;

/// A short spoken clip (macOS `say`, 22.05 kHz mono PCM16) so the decode →
/// resample → mix path can be exercised without the pipeline.
const TEST_CLIP_WAV: &[u8] = include_bytes!("../assets/lupo-test.wav");

/// When the dock URL was last copied, so the row can confirm it briefly.
static DOCK_COPIED_AT: Mutex<Option<std::time::Instant>> = Mutex::new(None);
const DOCK_COPIED_FEEDBACK: Duration = Duration::from_secs(5);

/// Per-instance state; `create` boxes it and OBS hands the pointer back to
/// every other callback.
struct LupoSource {
    handle: SourceHandle,
    label: String,
    player: Arc<Player>,
    /// Output channel claimed while `global` is on.
    channel: Mutex<Option<u32>>,
}

impl LupoSource {
    /// # Safety
    /// `data` must be a pointer previously returned by [`create`] that has not
    /// been passed to [`destroy`].
    unsafe fn from_data<'a>(data: *mut c_void) -> Option<&'a LupoSource> {
        // SAFETY: guaranteed by the caller contract above.
        unsafe { data.cast::<LupoSource>().as_ref() }
    }

    fn target_id(data: *mut c_void) -> TargetId {
        data as usize
    }

    fn set_global(&self, global: bool) {
        let mut channel = self.channel.lock().unwrap_or_else(|e| e.into_inner());
        match (global, *channel) {
            (true, None) => match claim_output_channel(self.handle.0) {
                Some(claimed) => {
                    info!(source = %self.label, channel = claimed, "global output enabled");
                    *channel = Some(claimed);
                }
                None => warn!(
                    source = %self.label,
                    "no free OBS output channel; global output unavailable"
                ),
            },
            (false, Some(claimed)) => {
                release_output_channel(claimed);
                info!(source = %self.label, channel = claimed, "global output disabled");
                *channel = None;
            }
            _ => {}
        }
    }

    fn release_channel(&self) {
        let mut channel = self.channel.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(claimed) = channel.take() {
            release_output_channel(claimed);
            info!(source = %self.label, channel = claimed, "global output channel released");
        }
    }

    fn apply_settings(&self, settings: *mut sys::obs_data_t) {
        // SAFETY: `settings` is a live obs_data_t for the duration of the callback.
        let global = unsafe { sys::obs_data_get_bool(settings, SETTING_GLOBAL.as_ptr()) };
        self.set_global(global);
    }
}

/// # Safety
/// `settings` must be a live `obs_data_t`.
unsafe fn set_data_string(settings: *mut sys::obs_data_t, name: &CStr, value: &str) {
    let value = std::ffi::CString::new(value.replace('\0', " ")).unwrap_or_default();
    // SAFETY: caller contract; both strings are NUL-terminated and outlive the call.
    unsafe { sys::obs_data_set_string(settings, name.as_ptr(), value.as_ptr()) };
}

/// Serializes our own claims; the read-back below guards against other plugins
/// racing `obs_set_output_source` on the same channel.
static CLAIM_LOCK: Mutex<()> = Mutex::new(());

fn claim_output_channel(source: *mut sys::obs_source_t) -> Option<u32> {
    let _guard = CLAIM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for channel in (FIRST_CLAIMABLE_CHANNEL..OBS_MAX_CHANNELS).rev() {
        // SAFETY: plain libobs calls; `obs_get_output_source` returns a new
        // reference that must be released.
        unsafe {
            let existing = sys::obs_get_output_source(channel);
            if !existing.is_null() {
                sys::obs_source_release(existing);
                continue;
            }
            sys::obs_set_output_source(channel, source);
            // Check the claim stuck: someone else may have taken the channel
            // between the emptiness check and the set.
            let now = sys::obs_get_output_source(channel);
            let ours = std::ptr::eq(now, source);
            if !now.is_null() {
                sys::obs_source_release(now);
            }
            if ours {
                return Some(channel);
            }
        }
    }
    None
}

fn release_output_channel(channel: u32) {
    // SAFETY: clearing a channel is always valid, even if OBS already did.
    unsafe { sys::obs_set_output_source(channel, std::ptr::null_mut()) };
}

fn source_label(source: *mut sys::obs_source_t) -> String {
    // SAFETY: `source` is a live source handed to us by libobs; the returned
    // string is owned by the source and copied before we return.
    unsafe {
        let name = sys::obs_source_get_name(source);
        if name.is_null() {
            "<unnamed>".to_string()
        } else {
            CStr::from_ptr(name).to_string_lossy().into_owned()
        }
    }
}

unsafe extern "C" fn get_name(_type_data: *mut c_void) -> *const c_char {
    SOURCE_NAME.as_ptr()
}

unsafe extern "C" fn get_defaults(settings: *mut sys::obs_data_t) {
    // SAFETY: `settings` is a live obs_data_t.
    unsafe {
        sys::obs_data_set_default_bool(settings, SETTING_GLOBAL.as_ptr(), false);
        sys::obs_data_set_default_bool(settings, SETTING_DEBUG_TOOLS.as_ptr(), false);
    }
}

unsafe extern "C" fn create(
    settings: *mut sys::obs_data_t,
    source: *mut sys::obs_source_t,
) -> *mut c_void {
    ffi_guard("lupo_tts.create", std::ptr::null_mut(), || {
        let label = source_label(source);
        let handle = SourceHandle(source);
        let player = Arc::new(Player::spawn(handle, label.clone()));
        let data = Box::into_raw(Box::new(LupoSource {
            handle,
            label: label.clone(),
            player: player.clone(),
            channel: Mutex::new(None),
        }));
        // SAFETY: `source` is live; `data` stays valid until `destroy`, which
        // disconnects both callbacks before freeing it.
        unsafe {
            sys::obs_source_add_audio_capture_callback(source, Some(audio_capture), data.cast());
            sys::signal_handler_connect(
                sys::obs_source_get_signal_handler(source),
                c"remove".as_ptr(),
                Some(on_remove),
                data.cast(),
            );
        }
        match engine::get() {
            Some(engine) => engine
                .sink
                .register(LupoSource::target_id(data.cast()), player),
            None => warn!(source = %label, "engine not running; only test buttons will work"),
        }
        // SAFETY: `data` was just created and is valid.
        unsafe { (*data).apply_settings(settings) };
        info!(source = %label, "source created");
        data.cast()
    })
}

/// "remove" fires when the user deletes the source (or the collection is torn
/// down). A claimed output channel holds a reference, so release it here or
/// the source would never reach `destroy`.
unsafe extern "C" fn on_remove(data: *mut c_void, _calldata: *mut sys::calldata_t) {
    ffi_guard("lupo_tts.remove", (), || {
        // SAFETY: OBS passes the pointer we connected with; the source is alive.
        if let Some(source) = unsafe { LupoSource::from_data(data) } {
            info!(source = %source.label, "source removed");
            source.release_channel();
        }
    })
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    ffi_guard("lupo_tts.destroy", (), || {
        if data.is_null() {
            return;
        }
        // SAFETY: `data` came from `create` and OBS calls destroy exactly once.
        let boxed = unsafe { Box::from_raw(data.cast::<LupoSource>()) };
        // SAFETY: the source is still alive during `destroy`.
        unsafe {
            sys::signal_handler_disconnect(
                sys::obs_source_get_signal_handler(boxed.handle.0),
                c"remove".as_ptr(),
                Some(on_remove),
                data,
            );
            sys::obs_source_remove_audio_capture_callback(
                boxed.handle.0,
                Some(audio_capture),
                data,
            );
        }
        if let Some(engine) = engine::get() {
            engine.sink.unregister(LupoSource::target_id(data));
        }
        boxed.release_channel();
        // Join the player thread explicitly before the source pointer dies. A
        // concurrent sink dispatch may still hold an `Arc<Player>` clone, so
        // dropping `boxed` is not guaranteed to be the last reference — relying
        // on it would let the thread keep pushing audio into a freed source.
        boxed.player.shutdown();
        let stats = boxed.player.stats();
        info!(
            source = %boxed.label,
            clips_played = stats.clips_played.load(Ordering::Relaxed),
            pushed_frames = stats.pushed_frames.load(Ordering::Relaxed),
            consumed_frames = stats.consumed_frames.load(Ordering::Relaxed),
            "source destroyed"
        );
        drop(boxed);
    })
}

unsafe extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    ffi_guard("lupo_tts.update", (), || {
        // SAFETY: OBS passes the pointer returned by `create`.
        if let Some(source) = unsafe { LupoSource::from_data(data) } {
            source.apply_settings(settings);
        }
    })
}

macro_rules! lifecycle_callback {
    ($name:ident, $what:literal) => {
        unsafe extern "C" fn $name(data: *mut c_void) {
            ffi_guard(concat!("lupo_tts.", $what), (), || {
                // SAFETY: OBS passes the pointer returned by `create`.
                if let Some(source) = unsafe { LupoSource::from_data(data) } {
                    info!(source = %source.label, concat!("source ", $what));
                }
            })
        }
    };
}

lifecycle_callback!(activate, "activate");
lifecycle_callback!(deactivate, "deactivate");
lifecycle_callback!(show, "show");
lifecycle_callback!(hide, "hide");

/// Called by OBS on its audio thread with our source's audio *after* OBS
/// processed it (resampled to the output rate, filters applied). Counting here
/// tells us whether pushed audio actually reaches the mixer.
unsafe extern "C" fn audio_capture(
    param: *mut c_void,
    _source: *mut sys::obs_source_t,
    audio_data: *const sys::audio_data,
    muted: bool,
) {
    ffi_guard("lupo_tts.audio_capture", (), || {
        // SAFETY: `param` is our `create` pointer, valid until destroy removes
        // this callback; `audio_data` is valid for the call.
        let (Some(source), Some(audio)) = (unsafe { LupoSource::from_data(param) }, unsafe {
            audio_data.as_ref()
        }) else {
            return;
        };
        let stats = source.player.stats();
        stats
            .consumed_frames
            .fetch_add(u64::from(audio.frames), Ordering::Relaxed);
        if muted {
            stats
                .consumed_muted_frames
                .fetch_add(u64::from(audio.frames), Ordering::Relaxed);
        }
    })
}

fn output_sample_rate() -> u32 {
    let mut info = sys::obs_audio_info {
        samples_per_sec: 48_000,
        speakers: sys::SPEAKERS_STEREO,
    };
    // SAFETY: `info` is a valid out-pointer for the duration of the call.
    if unsafe { sys::obs_get_audio_info(&mut info) } {
        info.samples_per_sec
    } else {
        48_000
    }
}

macro_rules! button_callback {
    ($name:ident, $what:literal, |$source:ident| $body:block) => {
        unsafe extern "C" fn $name(
            _props: *mut sys::obs_properties_t,
            _property: *mut sys::obs_property_t,
            data: *mut c_void,
        ) -> bool {
            ffi_guard(concat!("lupo_tts.", $what), false, || {
                // SAFETY: OBS passes the source's private data to button callbacks.
                if let Some($source) = unsafe { LupoSource::from_data(data) } $body
                false
            })
        }
    };
}

button_callback!(on_test_tone, "test_tone", |source| {
    let rate = output_sample_rate();
    info!(source = %source.label, rate, "test tone requested");
    // Test buttons do not wait for completion.
    drop(source.player.play(Arc::new(tone(440.0, 1.0, rate, 0.5))));
});

button_callback!(on_test_clip, "test_clip", |source| {
    match lupo_audio::decode_to_pcm(TEST_CLIP_WAV) {
        Ok(pcm) => {
            info!(
                source = %source.label,
                sample_rate = pcm.sample_rate,
                channels = pcm.channels,
                clip_ms = pcm.duration().as_millis() as u64,
                "test clip requested"
            );
            drop(source.player.play(Arc::new(pcm)));
        }
        Err(err) => warn!(source = %source.label, %err, "failed to decode test clip"),
    }
});

button_callback!(on_stop, "stop", |source| {
    source.player.stop();
});

button_callback!(on_dock_copy, "dock_copy", |source| {
    let Some(url) = engine::get().and_then(|engine| engine.dock_url.clone()) else {
        return false;
    };
    match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(url)) {
        Ok(()) => {
            info!(source = %source.label, "dock url copied to the clipboard");
            *DOCK_COPIED_AT.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(std::time::Instant::now());
        }
        Err(err) => warn!(source = %source.label, %err, "failed to copy the dock url"),
    }
    return true;
});

button_callback!(on_debug_panic, "debug_panic", |source| {
    let label = source.label.clone();
    panic!("deliberate panic from the debug button on {label}");
});

unsafe extern "C" fn get_properties(data: *mut c_void) -> *mut sys::obs_properties_t {
    ffi_guard("lupo_tts.get_properties", std::ptr::null_mut(), || {
        let engine = engine::get();
        let engine_status = match &engine {
            Some(engine) => format!(
                "running — {} source instance(s) receive audio",
                engine.sink.target_count()
            ),
            None => "not running — check the OBS log".to_string(),
        };
        let dock_row = match engine
            .as_ref()
            .and_then(|engine| engine.dock_url.as_deref())
        {
            Some(url) => {
                let copied_recently = DOCK_COPIED_AT
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some_and(|at| at.elapsed() < DOCK_COPIED_FEEDBACK);
                let address = url.split('?').next().unwrap_or(url);
                let manual = engine
                    .as_ref()
                    .and_then(|engine| engine.dock_manual_reason.as_deref());
                match (copied_recently, manual) {
                    (true, _) => format!("{address} — URL copied to the clipboard ✓"),
                    (false, Some(reason)) => format!(
                        "{address} — not added automatically ({reason}); press \"Copy dock URL\" and add a Custom Browser Dock"
                    ),
                    (false, None) => {
                        format!("added to the Docks menu (Docks → LUPO TTS) · {address}")
                    }
                }
            }
            None => "dock server not running — check the OBS log".to_string(),
        };
        // Info rows display the value stored under their key.
        // SAFETY: OBS passes the pointer returned by `create`; the source is alive.
        let debug_tools = unsafe {
            match LupoSource::from_data(data) {
                Some(source) => {
                    let settings = sys::obs_source_get_settings(source.handle.0);
                    set_data_string(settings, SETTING_ENGINE_STATUS, &engine_status);
                    set_data_string(settings, SETTING_DOCK_URL, &dock_row);
                    let debug = sys::obs_data_get_bool(settings, SETTING_DEBUG_TOOLS.as_ptr());
                    sys::obs_data_release(settings);
                    debug
                }
                None => false,
            }
        };
        // SAFETY: plain libobs property-building calls; OBS owns the result.
        unsafe {
            let props = sys::obs_properties_create();
            sys::obs_properties_add_text(
                props,
                SETTING_ENGINE_STATUS.as_ptr(),
                c"Engine".as_ptr(),
                sys::OBS_TEXT_INFO,
            );
            let dock = sys::obs_properties_add_text(
                props,
                SETTING_DOCK_URL.as_ptr(),
                c"Dock".as_ptr(),
                sys::OBS_TEXT_INFO,
            );
            sys::obs_property_set_long_description(
                dock,
                c"Everything (queue, Twitch login, voices, settings) lives in the LUPO TTS dock. If it is missing from the Docks menu, press \"Copy dock URL\" and add it via Docks → Custom Browser Docks.".as_ptr(),
            );
            sys::obs_properties_add_button(
                props,
                c"dock_copy".as_ptr(),
                c"Copy dock URL".as_ptr(),
                Some(on_dock_copy),
            );
            let global = sys::obs_properties_add_bool(
                props,
                SETTING_GLOBAL.as_ptr(),
                c"Keep audible in every scene".as_ptr(),
            );
            sys::obs_property_set_long_description(
                global,
                c"Claims an OBS output channel so this source is mixed regardless of which scene is live. Off: the source is only heard in scenes that contain it.".as_ptr(),
            );
            sys::obs_properties_add_bool(
                props,
                SETTING_DEBUG_TOOLS.as_ptr(),
                c"Show debug tools".as_ptr(),
            );
            if debug_tools {
                sys::obs_properties_add_button(
                    props,
                    c"test_tone".as_ptr(),
                    c"Play test tone (bypasses the pipeline)".as_ptr(),
                    Some(on_test_tone),
                );
                sys::obs_properties_add_button(
                    props,
                    c"test_clip".as_ptr(),
                    c"Play test clip (bypasses the pipeline)".as_ptr(),
                    Some(on_test_clip),
                );
                sys::obs_properties_add_button(
                    props,
                    c"stop".as_ptr(),
                    c"Stop test playback".as_ptr(),
                    Some(on_stop),
                );
                sys::obs_properties_add_button(
                    props,
                    c"debug_panic".as_ptr(),
                    c"Panic inside a callback (must be logged, not crash OBS)".as_ptr(),
                    Some(on_debug_panic),
                );
            }
            props
        }
    })
}

/// Register the source type with libobs. Call once from `obs_module_load`.
pub fn register() {
    let info = Box::leak(Box::new(sys::obs_source_info {
        id: SOURCE_ID.as_ptr(),
        type_: sys::OBS_SOURCE_TYPE_INPUT,
        output_flags: sys::OBS_SOURCE_AUDIO,
        get_name: Some(get_name),
        create: Some(create),
        destroy: Some(destroy),
        get_defaults: Some(get_defaults),
        update: Some(update),
        get_properties: Some(get_properties),
        activate: Some(activate),
        deactivate: Some(deactivate),
        show: Some(show),
        hide: Some(hide),
        icon_type: sys::OBS_ICON_TYPE_AUDIO_OUTPUT,
        ..Default::default()
    }));
    // SAFETY: `info` is 'static and fully initialised; libobs copies it.
    unsafe { sys::obs_register_source_s(info, std::mem::size_of::<sys::obs_source_info>()) };
    info!(
        id = SOURCE_ID.to_string_lossy().as_ref(),
        "source type registered"
    );
}

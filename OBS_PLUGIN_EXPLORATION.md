# OBS Plugin Exploration

Status: **experimenting** — nothing here is committed design until the "UI decision" gate
(Round 5) is passed and the outcome is folded into `DESIGN.md`. This file is a living lab
notebook: findings, decisions, and the experiment rounds with their pass/fail criteria.
Update it as rounds complete; retire it once `DESIGN.md` owns the result.

## 1. Goal

A second frontend for LUPO TTS, parallel to the Dioxus desktop shell in `app/`, that runs
**inside OBS Studio as a native plugin**. The plugin hosts the existing message pipeline
in-process and exposes TTS playback as an OBS **audio source**, so the speech lands in the
mixer and is recorded/streamed like any other source, with no virtual audio cable.

Scope decisions (2026-08-27):

- Design space is **A: the plugin hosts the pipeline** (see §5 for the alternatives that
  were considered and why they are only fallbacks).
- **macOS-first for development** (what we have on hand: OBS 31.0.3, arm64), but **1.0
  targets Windows with equal support**; Linux is optional. Every choice below is checked
  against the platform matrix in §8 so we don't paint ourselves into a macOS corner.
- The desktop app and the plugin are **both shipped for now**. They do not need to run
  concurrently on the same machine, but they must not corrupt each other's state.

Non-goals for the exploration phase: Windows/Linux builds, on-stream overlays ("now
playing" text/video source), any UI beyond OBS's Properties dialog, packaging/signing for
distribution.

## 2. What the codebase already gives us

Facts that shape the design (paths as of 2026-08-27):

- **The backend is frontend-agnostic.** As of Round 0 it is its own crate, `runtime/`
  (`lupo-runtime`), with zero UI dependencies. The whole backend is
  `MessagePipeline::start(runtime, db, audio, settings)` (`runtime/src/pipeline.rs`), wired
  once in `app/src/app/model.rs` (~line 1800). Frontends receive `RuntimeEvent`s through
  `RuntimeBridge`, a plain `Arc<Mutex<VecDeque<_>>>`. The plugin reuses Twitch/YouTube
  ingestion, the lifecycle state machine, persistence and all TTS providers unchanged.
- **Audio was the one coupling point; now abstracted.** The pipeline plays through
  `Arc<dyn lupo_audio::AudioSink>`; `DeviceSink` (rodio, feature `device`) is the desktop
  implementation and `decode_to_pcm` yields interleaved f32 PCM for hosts that push samples
  themselves. OBS is the inverse of a device: an audio source *pushes* PCM frames into OBS
  (`obs_source_output_audio`) at the rate `obs_get_audio_info()` reports (typically
  48 kHz). See §6.
- **Persistence is single-process.** One redb file under `app_project_dirs()` holds
  messages, settings **and secrets**; redb takes an exclusive lock. The plugin therefore
  owns its own database (under `obs_module_config_path()`), and `lupo-cli` needs a way to
  target it (see §7).
- **Twitch auth already uses the Device Code flow** (`messages-twitch/src/auth.rs`,
  `cli/src/twitch.rs`): no localhost redirect server, which is exactly what a plugin
  running inside someone else's process wants.
- **`[profile.release]` sets `panic = "abort"`.** Inside OBS a panic kills the host. The
  plugin must `catch_unwind` at every FFI boundary and probably build with its own profile.
- `lupo_runtime::install_crypto_provider()` must run before any TLS use; in the
  plugin that means inside `obs_module_load`.

## 3. What OBS gives us

Verified against the local install (`/Applications/OBS.app`, OBS 31.0.3, arm64):

- **Plugin = C ABI shared library** exporting `obs_module_load`, `obs_module_unload`,
  `obs_module_ver` (via `OBS_DECLARE_MODULE`, which we replicate by hand in Rust),
  optionally `obs_module_set_pointer`, `obs_module_name`, `obs_module_description`,
  `obs_module_post_load`. Sources are registered with `obs_register_source_s(&info, size)`.
- **Audio source contract:** set `OBS_SOURCE_AUDIO` in `obs_source_info.output_flags`, push
  frames from any thread with `obs_source_output_audio(source, &obs_source_audio)`
  (`data[]`, `frames`, `speakers`, `format`, `samples_per_sec`, `timestamp` in ns). OBS
  resamples/remixes if the frame's format differs from `obs_get_audio_info()`, but we
  should match it to avoid surprises. Thread-safe; the usual pattern is a dedicated
  producer thread emitting 10–20 ms chunks with monotonic timestamps.
- **Settings UI:** `obs_properties_t` is *declarative*; OBS renders Qt widgets for us.
  Available widgets: bool, int/float (spin or slider), text (default / password /
  multiline / info-with-severity), path, list (combo / editable / radio), color, font,
  editable list, frame rate, group, button (callback may mutate the properties tree and
  return `true` to refresh). Values arrive in `obs_data_t` via `update()`. No custom
  widgets, no layout control. **Everything in Properties is saved in plaintext** in the
  scene collection JSON — never put secrets there.
- **Docks:** `obs_frontend_add_dock_by_id(id, title, QWidget*)` — a **Qt (C++) widget
  pointer**. OBS 31 bundles Qt 6.6; a Rust dock would mean cxx-qt/cxx against OBS's exact
  Qt ABI per release. Not a path we want. `obs-browser` supports user-added **Custom
  Browser Docks** (Docks → Custom Browser Docks → URL), which is the pragmatic escape hatch
  for a richer UI (§6.3).
- **Video sources get a canvas** (`video_render` on the graphics thread with the `gs_*`
  API, or async frames via `obs_source_output_video`) and can opt into interaction
  (`OBS_SOURCE_INTERACTION`: mouse/key callbacks through the *Interact* window). That is a
  buffer to draw *into the scene*, not a control-panel surface.
- **macOS specifics:** plugin bundles live in
  `~/Library/Application Support/obs-studio/plugins/<name>.plugin/Contents/{Info.plist,MacOS/<name>,Resources/locale/*.ini}`,
  `CFBundlePackageType = BNDL`, linked against `@rpath/libobs.framework/Versions/A/libobs`.
  OBS.app is Hardened Runtime **with** `com.apple.security.cs.disable-library-validation`,
  so an ad-hoc-signed Rust cdylib loads (confirmed in Round 1). The shipped framework has
  **no headers** → bindgen runs against the `vendor/obs-studio` submodule (shallow, pinned
  to the matching tag). Module discovery (`libobs/obs-module.c`) globs
  `<dir>/%module%.plugin/Contents/MacOS/` with an empty extension, so the executable must
  carry the bundle's base name (`lupo.plugin/Contents/MacOS/lupo`).
- **Version gate:** OBS 31 only `dlsym`s `obs_module_ver` and logs it; there is no
  major-version rejection in `obs-module.c`. We still report the exact `LIBOBS_API_VER`
  the bindings were generated from.
- **Linking:** the plugin is linked with `-undefined dynamic_lookup` (macOS); symbols
  resolve against the OBS process at load. No libobs on the build machine, CI stays green.
  Linux gets the same for free (shared objects may have undefined symbols). Windows needs
  an import library; it can be produced from the shipped `obs.dll` with `lib.exe /def` or
  `dlltool` without building OBS.
- **Logging:** `blog(level, fmt, ...)` — route `tracing` into it so plugin logs appear in
  OBS's log files/viewer.
- **Rust bindings ecosystem:** `obs-sys`/`obs-wrapper` (bennetthardwick) is the only
  "write a plugin" wrapper and was **archived Dec 2024**. `libobs-rs`/`libobs-wrapper` are
  maintained but solve the opposite problem (embedding libobs). Decision: our own minimal
  `obs-sys` (§4).

## 4. Target architecture

```
workspace
├── runtime/        lupo-runtime   (done in Round 0; no UI deps)
├── audio/          lupo-audio     (`AudioSink` trait, `DeviceSink` behind feature `device`, `decode_to_pcm`)
├── app/            lupo-app       (Dioxus shell; depends on lupo-runtime, uses DeviceSink)
├── obs-sys/        lupo-obs-sys   (done in Round 1: raw FFI, bindgen output committed per triple)
├── obs/            lupo-obs       (Round 1: module entry points + tracing→blog; later: source, ObsSink)
└── xtask/                          (`bundle-obs [--release] [--install]`, `obs-bindgen`)
```

Data flow inside OBS:

```
 obs_module_load ──► install_crypto_provider
                 ──► tokio Runtime (multi-thread)
                 ──► open redb at obs_module_config_path()
                 ──► SettingsService / secrets on that db
                 ──► MessagePipeline::start(rt, db, Arc<ObsSink>, settings)
                 ──► obs_register_source_s("lupo_tts")

 Twitch/YouTube ──► pipeline (filter → transform → synth → playback worker)
                                                            │ play_and_wait(bytes)
                                                            ▼
                                                 ObsSink: decode → resample → 20 ms chunks
                                                            │ obs_source_output_audio
                                                            ▼
                                                 OBS mixer → monitor / record / stream

 Properties dialog ──► update(obs_data) ──► pipeline/settings commands
 Properties buttons ──► "Test message", "Login to Twitch" (device code), "Clear queue"
```

Rules of the road for `lupo-obs`:

- Every `extern "C"` callback wraps its body in `std::panic::catch_unwind` and logs; a
  panic must never unwind into libobs.
- No blocking on the OBS UI thread: Properties callbacks hand work to the tokio runtime
  and return; results come back through the existing `RuntimeBridge`/events and are
  surfaced as info-text properties on the next refresh.
- One `MessagePipeline` per **module**, not per source instance. Multiple `lupo_tts`
  sources (e.g. one per scene) all attach to the same pipeline; only the *active* source
  instance emits audio (decide policy in Round 2).
- `obs_module_unload` must stop ingestion, shut the tokio runtime down with a timeout,
  and close the db. OBS hanging on quit is a Round 3 failure.

## 5. Alternatives considered

| | A. Plugin hosts the pipeline (chosen) | B. Plugin is an audio bridge | C. No plugin |
|---|---|---|---|
| Pipeline runs | inside OBS | in desktop `lupo` | in desktop `lupo` |
| Audio path | `ObsSink` → OBS source | local socket → OBS source | virtual audio cable |
| UI | Properties (→ browser dock later) | full Dioxus UI | full Dioxus UI |
| State | plugin-owned db | unchanged | unchanged |

C is the bar A must beat. B stays as the fallback if Round 3 shows that hosting the
pipeline in-process is unworkable; Rounds 0–2 are shared with B, so nothing is wasted.

## 6. Design notes on the three big questions

### 6.1 Twitch authentication

OBS's own Twitch login (Settings → Stream → Connect Account) stores `Token`/`RefreshToken`
in the profile's `basic.ini` `[Twitch]` section. It is **not reusable**: there is no
frontend API that exposes it; the token belongs to OBS's client ID and is refreshed through
OBS's proxy (`auth-api.obsproject.com`), so we could neither refresh it nor request the
scopes we need (`user:read:chat` for EventSub chat); and using another app's token is
against Twitch's developer agreement. The plugin runs lupo's existing **Device Code flow**:
a "Login to Twitch" button starts it, the verification URL + user code are shown in an
info-text property (plus an "Open browser" button), and the runtime polls to completion.
Tokens go into the plugin's own db via the existing `TwitchTokenStorage` trait.

### 6.2 Audio abstraction

Yes — the trait lives in `lupo-audio`, the OBS implementation lives in `lupo-obs`.
Shape as landed in Round 0 (`audio/src/lib.rs`):

```rust
// lupo-audio — boxed futures keep the trait object-safe, same pattern as
// messages-twitch's StorageFuture; no async-trait dependency.
pub type SinkFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;
pub trait AudioSink: Send + Sync + Debug {
    fn play_wav(&self, audio: Bytes) -> SinkFuture<'_, ()>;          // enqueue
    fn play_wav_and_wait(&self, audio: Bytes) -> SinkFuture<'_, ()>; // default: play + drain
    fn stop(&self) -> SinkFuture<'_, ()>;
    fn drain(&self) -> SinkFuture<'_, ()>;
    fn set_volume(&self, v: f32) -> SinkFuture<'_, f32>;
    fn volume(&self) -> SinkFuture<'_, f32>;
}
pub struct DeviceSink;                       // feature "device" (default): rodio thread
pub struct PcmBuffer { sample_rate, channels, samples: Vec<f32> /* interleaved */ }
pub fn decode_to_pcm(bytes: impl Into<Bytes>) -> Result<PcmBuffer, Error>; // no device
```

`decode_to_pcm` (and a resampler helper, if one turns out to be needed) is generic and
belongs in `lupo-audio`; `ObsSink` in `lupo-obs` consumes `PcmBuffer` and knows about
`obs_source_audio`, timestamps and chunking. The pipeline takes `Arc<dyn AudioSink>`.
rodio's `playback` feature (cpal/CoreAudio) is only enabled through `lupo-audio`'s
`device` feature; `lupo-runtime` depends on `lupo-audio` with default features off, so
`cargo tree -p lupo-runtime` contains no cpal. Feature unification only bites when the
plugin and the desktop app are built in the same cargo invocation, which is harmless.

### 6.3 UI freedom

There is no "buffer to draw a control panel on". The options, in order of effort:

1. **Properties dialog** (declarative widgets, zero custom drawing). Enough for: enable
   toggle, Twitch channel, YouTube URL, provider/voice pick-lists, volume, test message,
   clear queue, login. Cross-platform for free. **MVP uses this.**
2. **Custom Browser Dock** (obs-browser/CEF, present on Windows/macOS and most Linux
   builds) pointing at `http://127.0.0.1:<port>` served by an HTTP/WebSocket server inside
   the plugin. This is the route to **reuse `app/src/app`**: Dioxus renders to web, so
   `ui.rs`/`state.rs` components carry over; `model.rs` currently calls `AppServices`
   in-process and would need a transport layer (the `RuntimeBridge` events + action enum
   are already the natural wire protocol). Decision deferred to Round 5, after living with
   option 1. Note the dock may be user-added via the menu (no API); programmatic creation
   through obs-browser is an open question (§9).
3. **Native Qt dock** via cxx-qt against OBS's bundled Qt 6.6 ABI. Rejected: C++ in the
   build, per-OBS-release Qt pinning, three platforms.
4. **Render the UI into a video source** and drive it via the Interact window
   (`OBS_SOURCE_INTERACTION`). Technically "a buffer to draw on", but it puts the control
   panel *in the scene*, input is laggy and modal, and dioxus-native/blitz renders through
   wgpu (offscreen readback needed). Not for control UI; maybe relevant later for an
   on-stream "now playing" overlay rendered from Dioxus components.

## 7. Secrets and state

- Plugin db: `<obs_module_config_path()>/lupo.redb` — per-user, per-OBS-install, and
  separate from the desktop app's `app_project_dirs()` db. No sharing, no lock conflict.
- `lupo-cli` gains a `--db <path>` (or `--profile obs`) flag so `lupo-cli secret set …`
  and `lupo-cli twitch login` can target the plugin db while OBS is **closed** (exclusive
  lock). Longer term the plugin should not need the CLI at all: Twitch login via device
  code from Properties; API keys via a `password` text property whose value is moved into
  the secret store on `update()` and **cleared from `obs_data`** so it never hits the scene
  collection JSON. Validate this pattern in Round 4.

## 8. Platform matrix (for keeping 1.0 honest)

| | macOS | Windows | Linux |
|---|---|---|---|
| Artifact | `lupo.plugin` bundle (`Contents/MacOS/lupo`, `Info.plist`, `Resources/locale`) | `lupo.dll` + `data/` | `lupo.so` + `data/` |
| Install dir | `~/Library/Application Support/obs-studio/plugins/` | `%ProgramData%\obs-studio\plugins\lupo\bin\64bit\` | `~/.config/obs-studio/plugins/lupo/bin/64bit/` |
| Link target | `@rpath/libobs.framework` | `obs.lib` import library (from obs-studio build/SDK) | `libobs.so.31` |
| Headers for bindgen | obs-studio source at matching tag | same | same |
| Signing | ad-hoc for dev; Developer ID + notarization for release | Authenticode optional | none |
| Rust target | `aarch64-apple-darwin` (+ `x86_64` for universal) | `x86_64-pc-windows-msvc` | `x86_64-unknown-linux-gnu` |
| TTS providers | OpenAI, Parler, macOS system | OpenAI, Parler (SAPI later) | OpenAI, Parler |

Keep `lupo-obs-sys` layout platform-neutral from Round 1: `bindings/<target>.rs` committed,
`build.rs` only picks the link flags.

## 9. Open questions

- [x] Does OBS 31 load an ad-hoc-signed cdylib bundle? **Yes** (Round 1).
- [x] `obs_module_ver` compatibility: OBS 31 does not gate on it (Round 1). Pin the
      submodule to the oldest OBS we support once that is decided; bindings are per
      OBS release *and* per triple.
- [x] Frame/timestamp discipline (Round 2): 10 ms chunks stamped `start + n·chunk`, pushed
      20 ms ahead against `os_gettime_ns()`, are accepted as direct timestamps — no
      "adding N ms of audio buffering" for our source, no TS-smoothing warnings. OBS
      resamples/remixes itself when the clip's rate/channels differ from the output
      (22.05 kHz mono → 48 kHz stereo verified: consumed = pushed × 2.18). Muted: OBS keeps
      consuming (capture callback `muted=true`), meter stays 0. Hidden scene item: OBS fires
      `hide` + `deactivate`, still consumes the frames, but nothing reaches the mix →
      **a source that is not visible in the program scene is silent in the recording**.
      "play_and_wait" = sleep until the last chunk's timestamp + ~50 ms; the capture
      callback confirms consumption.
- [x] Multiple instances (Round 2): each instance owns a player thread; two play
      independently; removing one mid-clip joins its thread cleanly (no crash, 0 leaks).
      Policy for the pipeline (Round 3): one pipeline per module, and **which instance
      emits** follows from the hidden-source finding — see the global-channel question.
- [x] Scene-bound vs global (decided 2026-08-27: **offer both**, Round 3): every
      `lupo_tts` instance is scene-bound by default; its "Keep audible in every scene"
      setting claims a free OBS output channel (`obs_set_output_source`, scanning 63 → 6)
      so the same source is mixed regardless of the live scene. A source active both via a
      scene item and a channel is mixed once. The channel holds a reference, so it is
      released on the source's `remove` signal (otherwise a deleted source would never be
      destroyed); OBS's `ClearSceneData` also nulls every channel on collection switch/exit.
- [ ] Clean unload: tokio shutdown timing, db close, OBS quit latency. (Round 3)
- [ ] dylib size and module load time with Parler/candle compiled in; feature-gate? (Round 3)
- [ ] Panic containment: does a caught panic leave the pipeline usable? (Round 3)
- [x] Clean unload (Round 3): the pipeline's workers never exit on their own (the core
      keeps a clone of its own command sender), so reclaiming an `Arc<Runtime>` never
      succeeds. Fix: the engine *owns* the `Runtime` and hands `tokio::runtime::Handle`s
      to the pipeline (API changed accordingly, desktop app adapted); unload drops the
      pipeline and calls `shutdown_timeout(3 s)`, which drops parked tasks immediately.
      Measured: **engine stopped in 42 ms**, OBS exits with 0 leaks.
- [x] Load cost (Round 3): engine start (crypto provider, runtime, redb open, pipeline,
      system-voice helper spawn) = **49 ms**; `obs_init_module` ≈ 1 ms. Debug dylib
      ≈ 9 MB; Parler/candle is compiled in but idle — feature-gating stays on the list for
      release sizing, not a blocker.
- [x] Panic containment (Round 4): a "Show debug tools" setting reveals a button that
      panics inside its callback; `ffi_guard` logs `panic caught at FFI boundary` and OBS
      keeps running (verified over websocket afterwards). Panics inside tokio tasks are
      caught per task by tokio; a worker that dies would degrade the pipeline silently —
      worth a supervisor/restart later, not an OBS-specific problem.
- [ ] `play_wav` with no `lupo_tts` instance currently fails the message
      (`playback_failed`); consider pausing playback instead. (Round 5)
- [x] Device-code login UX (Round 4): "Connect Twitch account" starts the flow on the
      engine runtime and returns `true` so OBS rebuilds the dialog; `get_properties` writes
      the live status into the source's settings (`twitch_status`, `engine_status`) because
      `OBS_TEXT_INFO` rows display the *value* under their key; the code and
      `https://www.twitch.tv/activate?device-code=…` appear in the dialog, the browser opens
      unless "Open the browser automatically" is off, and the poll completes in the
      background (Twitch polls every 5 s). "Refresh status" re-reads it. No live push into an
      open dialog — acceptable for Properties, one more argument for a richer UI in Round 5.
- [x] Secrets (Round 4): an `OBS_TEXT_PASSWORD` property; `update` moves a non-empty value
      into the plugin's secret store (`secret_openai_api_key`) and `obs_data_erase`s it, so
      it never reaches the scene-collection JSON (verified: `GetInputSettings` shows no key).
      "Forget OpenAI key" deletes it. The CLI path also works:
      `lupo-cli secret --data-dir "~/Library/Application Support/obs-studio/plugin_config/lupo" set open_ai_api_key …`
      and `lupo-cli twitch --data-dir … auth login` (OBS must be closed: redb lock).
- [ ] Can a browser dock be created programmatically by a third-party plugin? (Round 5)
- [ ] Windows: generate `obs.lib` from the installed `obs.dll` (`dlltool`/`lib.exe /def`)
      in `obs/build.rs` or xtask, or vendor one per OBS release? (post-exploration)

## 10. Experiment rounds

Each round answers specific questions and has a concrete pass criterion. Rounds 0 and 1
are independent and can run in parallel.

### Round 0 — Repo prep (no OBS)  `[x]` 2026-08-27

- Moved `app/src/runtime/` to `runtime/` as `lupo-runtime` (`git mv`, module paths
  unchanged); `install_crypto_provider()` and its guard test moved with it; `lupo-app`
  keeps only the Dioxus UI and its manifest shrank accordingly.
- `AudioSink` trait + `DeviceSink` (feature `device`) + `decode_to_pcm` in `lupo-audio`;
  pipeline and `lupo-cli` take/construct the trait object.
- **Pass (verified):** `cargo test --workspace` green (97 tests), clippy `-D warnings`
  clean, `cargo check -p lupo-runtime --no-default-features` builds without cpal, desktop
  `lupo` starts and logs normally.
- Notes for later rounds: `MessagePipeline::start` still takes an `Arc<tokio::Runtime>`
  it does not own — the plugin creates one in `obs_module_load` and must keep it alive.

### Round 1 — Hello OBS  `[x]` 2026-08-27

- `vendor/obs-studio` shallow submodule at `31.0.3`; `cargo xtask obs-bindgen` writes
  `obs-sys/src/bindings/<triple>.rs` (allowlist `obs_*`/`blog`/`OBS_*`/`LOG_*`, module
  entry points blocklisted, layout tests on: 6.4k lines, 972 fns).
- `obs/`: cdylib exporting the seven `OBS_DECLARE_MODULE` symbols; every entry point runs
  under `ffi_guard` (`catch_unwind` → `blog(LOG_ERROR)`); `tracing` layer formats
  `target: message k=v` into `blog` with a `[lupo]` prefix.
- `cargo xtask bundle-obs [--release] [--install]` builds (`obs-release` profile =
  release + `panic = "unwind"`), writes the `BNDL` plist, ad-hoc signs, installs.
- **Pass (verified):** OBS 31.0.3 log:
  `[lupo] lupo_obs: lupo module loaded version=0.0.1 libobs_api=31.0.3`, `lupo` under
  "Loaded Modules", `obs_init_module(lupo): 0.37 ms`; on quit
  `[lupo] lupo_obs: lupo module unloading` and `Number of memory leaks: 0`, no crash
  report. Release dylib is 344 KB. CI checkout switched to one-level submodules.
- Notes for later rounds: `obs_module_text`/locale not wired (no `Resources/locale`
  yet); the `tracing` layer has no filtering (OBS's own log level applies).

### Round 2 — Audio source  `[ ]`

- Register `lupo_tts` (`OBS_SOURCE_AUDIO`), producer thread emits a 440 Hz tone, then a
  bundled WAV, via `obs_source_output_audio`.
- **Pass:** mixer meter moves; a recording contains the audio; hide/mute/scene switch and
  create/destroy cycles don't crash or leak.
- **Answers:** frame/timestamp/format discipline, lifecycle, multi-instance policy.

### Round 3 — Pipeline inside OBS  `[x]` 2026-08-27  (gate: A vs B → **A confirmed**)

- `obs/src/engine.rs`: started from `obs_module_post_load` (after all modules, before the
  scene collection loads): `install_crypto_provider`, owned tokio runtime
  (`lupo-obs-tokio` threads), redb at `obs_module_get_config_path()` =
  `~/Library/Application Support/obs-studio/plugin_config/lupo/`, `SettingsService`,
  `MessagePipeline::start_with_options` with the macOS helper path pointed at
  `lupo.plugin/Contents/MacOS/lupo-tts-macos-helper` (new `PipelineOptions`; xtask now
  ships the helper in the bundle). A task mirrors `PipelineEvent`s into the OBS log.
- `obs/src/sink.rs`: `ObsSink: AudioSink` fans each clip out to every live `lupo_tts`
  instance (registered on create / unregistered on destroy), applies the pipeline's
  volume as pre-gain, and `drain` awaits per-instance completion signals from the
  players. No instance → `Error::Sink` (message ends as `playback_failed`).
- Properties gained: engine status line, the global toggle, a "Test message" text and
  "Queue test message (through the LUPO pipeline)" / "Stop current message" /
  "Clear queue" buttons.
- **Pass (verified, 11/11 via `obs/smoke/run.sh`):** message 1 went Arrived → Filtered →
  Transforming → Transformed → Synthesizing → Synthesized (0.7 s, `system_tts`) → Queued
  → Playing → Played, meter peak 0.63 on the source; global mode audible on an empty
  program scene (peak 0.50), scene mode silent there (0); engine start 49 ms; unload
  clean (see §9); `Number of memory leaks: 0`.
- Provider status at start: `system_tts Ready`, `openai Unavailable (API key missing)` —
  secrets delivery is Round 4.

### Round 4 — Live chat  `[x]` 2026-08-27; live chat verified by hand 2026-08-28

- `obs/src/chat.rs` `ChatSupervisor` (owned by the engine): one EventSub listener per OBS
  process via `lupo_runtime::twitch::spawn_listener`, restarted when the account or channel
  changes; status mirrored into the OBS log and the Properties status row; device-code
  login (`lupo_runtime::twitch::DeviceLogin`, new) and logout.
- Channel override lives where the desktop app keeps it — the `connections` setting's
  `twitch_channel_login` field — through new shared helpers in `lupo_runtime::twitch`
  (`load/store_channel_override`, `normalize_channel_login`); the desktop app now
  delegates to them.
- Properties gained: engine/Twitch status rows, channel field, Connect / Refresh /
  Forget buttons, browser toggle, OpenAI key (password) + Forget, debug tools.
- `spawn_listener` (Twitch and YouTube) takes a `Handle` like the pipeline.
- **Pass (verified via `obs/smoke/run.sh`):** key scrubbed from OBS settings; status "not
  logged in" without an account; Connect produced a real device code with the browser
  suppressed; panic contained. **Verified by hand (2026-08-28):** device-code login
  approved in the browser, status "reading #<channel> as <login>", chat messages queued
  and spoken through the source; OpenAI key stored via Properties and probed OK.

### Round 4b — Properties feedback pass  `[x]` 2026-08-28

First hands-on feedback on the dialog (linked account, queued chat, spoke through OBS):

- Long labels squeezed the layout → short labels, explanations moved to tooltips
  (`obs_property_set_long_description`).
- The device code was only visible in the browser → a "Device code" info row shows
  `CODE → URL` while a login is pending, and the tooltip tells the user to compare it.
- No feedback after connecting, a "Refresh" button that flickers, stale "connecting…" on
  reopen → the dialog now refreshes itself: `obs_source_update_properties()` (queued to
  the UI thread by the frontend) is called on every Twitch status / login / provider /
  playback change, so rows update live and Connect / Forget appear only when applicable.
  The Refresh button is gone.
- **Found in the log:** OBS applies text fields per keystroke, so typing a channel name
  restarted the listener once per character → the restart is debounced (1.5 s).
- **Found in the log:** "Stop current message" pauses the pipeline (`stop_now` sets
  `playback_enabled = false`) and nothing showed or undid that; queued messages silently
  waited. → "Playback" row (`paused — N waiting` / `playing message id` / `idle`) and a
  Pause/Resume button; the stop button is now labelled "Stop and pause".
- "How do I hear it?" → the pipeline test tooltip points at Advanced Audio Properties →
  Audio Monitoring; and "Test OpenAI key" (`MessagePipeline::probe_provider`, new)
  synthesises a phrase without playing it and reports `OK — … KB in … s` / `failed — …`
  in an "OpenAI" row, plus a "Voices" row with every provider's readiness.
- **Crash found by the user (pasting the API key):** `obs_source_update_properties()`
  called *on the UI thread* from inside `update()` makes the frontend reload the dialog
  synchronously, destroying the `QLineEdit` whose `textEdited` handler is still on the
  stack (SIGSEGV in `QLineEdit::textEdited`). Rule, now enforced in
  `source::refresh_all_properties`: reloads are always dispatched from the engine
  runtime (coalesced, 50 ms), so the frontend queues them behind the current event. The
  websocket harness never hit this because it presses buttons off the UI thread — manual
  clicks are a test dimension the harness cannot cover.
- **Harness lesson:** the smoke run used the real plugin database and deleted the user's
  stored OpenAI key while testing the scrub path. `run.sh` now moves
  `plugin_config/lupo` aside for the run and restores it on exit.

### Round 5 — UI decision  `[~]` spike built 2026-08-28; decision pending

Inputs (2026-08-28, from the user): the OBS surface should be **small and
streamer-facing** — the desktop app is where complexity lives; **no client/server split
for the desktop app**; **no Dioxus in the dock** — a plain page is enough (Dioxus only
earns its place if we ever render video cheaply, out of scope).

Spike (design 1 from §6.3, reduced to what was asked):
- `lupo_runtime::ui_protocol` (new): JSON over WebSocket, tagged by `type`.
  Server → client: `snapshot` (playback + every message + chat status line) on connect,
  then `message` / `playback` / `cleared` / `chat` deltas and `error`. Client → server:
  `pause`, `resume`, `stop`, `clear`, `skip {id}`, `replay {id}`, `play_next {id}`.
  Frontend-agnostic; nothing in it is OBS-specific.
- `obs/src/dock.rs`: axum server on `127.0.0.1:27055` (first free of ten), started with
  the engine; every request needs a token generated once into
  `plugin_config/lupo/dock-token` (a page on any website could otherwise open a
  WebSocket to localhost and drive the queue). `/` serves `obs/assets/dock.html`
  (vanilla JS, dark, ~150 lines): Now playing / Queue / Recent, Pause · Stop & pause ·
  Clear, per-message Skip / Next / Replay, chat status, auto-reconnect.
- Properties shows the URL in a "Dock" row; the user adds it once via Docks → Custom
  Browser Docks (OBS 31 has no API for a plugin to create a browser dock — checked
  obs-browser's plugin entry: only `browser_source` and a websocket vendor are
  registered).
- Harness: fetches the page with/without the token (403), connects to `/ws`, checks the
  snapshot, `pause` → `playback{enabled:false}`, a queued test message streaming
  `queued → … → played`, and an `error` for a bad id.

Round 5b (2026-08-28) — **native dock, no manual step.** The Custom Browser Dock flow is
what web dashboards without a plugin use; real plugins register a `QWidget` with
`obs_frontend_add_dock_by_id` and appear in the Docks menu by themselves. obs-browser
exposes its CEF widget through a small C++ "panel" API (`browser-panel.hpp`: an abstract
`QCef` with a virtual destructor + 8 pure virtuals, obtained from the module via
`obs_browser_create_qcef`). `obs/csrc/dock_shim.cpp` (~90 lines, built by the `cc` crate,
**no Qt headers, no Qt linking**) mirrors that vtable with opaque Qt pointers, does the
same `init_browser()` → `wait_for_browser_init()` → `create_widget(url)` sequence as
OBS's own `InitBrowserPanelSafeBlock`, and hands the widget to
`obs_frontend_add_dock_by_id("lupo_tts", "LUPO TTS", widget)` from `obs_module_post_load`
(UI thread; before OBS restores the dock layout, so position persists). The obs-browser
panel version is checked at run time (≥ 3); on builds without obs-browser the Properties
row falls back to the Copy-URL flow with the reason. Verified: log `native dock added to
the Docks menu`, 24/24 harness checks, clean unload. Cost accepted: a C++ compiler at
build time on every platform (already required on Windows for the import library), and a
dependency on obs-browser's vtable order (versioned upstream).

- **Crash on quit, found by the smoke harness's OBS instance:** the dock's CEF widget was
  destroyed with the main window after obs-browser had shut CEF down (SIGSEGV in the
  widget destructor via a posted `deleteLater`). OBS clears its *own* browser docks in
  `closeEvent` before firing `OBS_FRONTEND_EVENT_EXIT`, and only deletes the CEF context in
  the destructor — so the plugin now registers `obs_frontend_add_event_callback` and
  removes the dock on `OBS_FRONTEND_EVENT_EXIT` (`obs_frontend_remove_dock` deletes it
  synchronously). Rule for any future widget: tear it down on EXIT, never at unload.
  `run.sh` now fails if a crash report appears during the run or the log lacks the final
  leak count — which is how this should have been caught the first time.

- **Tabbing/overlap bug (user):** tabbed together with OBS's Twitch Chat dock, ours stayed
  on top and the Chat dock lost its frame. OBS's `BrowserDock` sets
  `Qt::WA_NativeWindow` on the *dock* so the CEF `NSView` is owned by the dock's native
  view; `obs_frontend_add_dock_by_id` creates a plain `OBSDock` without it. The shim now
  finds the dock (the widget's parent after `setWidget`, read through Qt 6's public
  `QObjectData` layout) and calls the exported `QWidget::setAttribute` — still no Qt
  headers. Second Qt-ABI dependency, both documented in `dock_shim.cpp`; the clean fix
  is an upstream "add browser dock" frontend API.

Still to decide (the actual gate), after the user has used the dock live:
- Is this surface sufficient for the OBS frontend for 1.0 (setup in Properties, operation
  in the dock), or does the dock need settings too (then Properties shrinks to a status)?
- Twitch account/channel are module-wide but live in a per-source dialog; with a dock
  present, do they move there?
- Then: write the "OBS plugin frontend" section of `DESIGN.md`, fold the platform plan
  (§8) into `IMPLEMENTATION_PLAN.md`, retire this file.

### Round 6 — Option 2 with the full Dioxus route  `[x]` 2026-08-30

Decisions from the user (2026-08-30): option 2 (dock carries operation *and* settings;
Properties shrinks to per-source concerns); reversing the earlier no-client/server call
for the desktop app is acceptable **if the benefits earn it**; transport is settled by
OBS's constraints — Tauri/Dioxus IPC cannot reach a CEF page we don't own, so the
loopback HTTP+WS server is the universal transport and shell IPC is only for OS
integration; try the full-Dioxus route (UI in Rust, shared protocol types end to end).

Built:
- `ui-protocol/` (`lupo-ui-protocol`): the wire protocol as a serde-only crate that
  compiles natively and to wasm; `lupo_runtime::ui_protocol` re-exports it plus the
  conversions from persistence/runtime types. Vocabulary now covers settings: structured
  `ChatView` (account/channel/connection/login with the device code), `ProviderView`,
  volume, `submit`, `twitch_login/logout`, `set_channel`, `set_secret/forget_secret`
  (enum-keyed, values transit the token-protected loopback WS only), `probe`.
- `dock-ui/` (`lupo-dock-ui`): Dioxus 0.7 web app (~450 lines of Rust) — header with
  pause/stop/clear/volume, Now playing / Queue / Recent, and a Settings panel (Twitch
  device-code login rendered live, channel, OpenAI key + probe, test message). A
  `use_coroutine` owns the WebSocket (auto-reconnect); protocol types are shared at
  compile time on both ends. Native builds are an empty stub so the workspace stays
  green off-wasm.
- Build: `cargo xtask build-dock-ui` compiles to wasm32 and runs wasm-bindgen **as a
  library** (`wasm-bindgen-cli-support`, version-locked to the workspace's
  wasm-bindgen — no external CLI); `bundle-obs` ships the output (806 KB wasm + JS +
  snippets tree) in `lupo.plugin/Contents/Resources/dock-ui/`, served by the plugin from
  `obs_get_module_data_path()`. `.cargo/config.toml` scopes `-C target-cpu=native` away
  from wasm.
- Properties slimmed to: engine status, dock row + Copy URL, global toggle, debug tools.
  The Twitch/key/test rows and the whole per-keystroke settings machinery are gone.
- **Pass (verified, 28/28):** all prior checks plus, over the dock protocol: snapshot
  with chat/providers/volume; volume round-trip; submit → spoken (peak 0.66) with the
  lifecycle streamed; set_secret → provider ready → probe returns a textual failure for
  a fake key → forget → unavailable; set_channel lands in the chat view; device-code
  login streams the code; path traversal on `/assets` rejected; clean exit, 0 leaks.
  The Dioxus app verified booting inside the real CEF dock (established WS connections
  from the OBS renderer process to the dock server).
- Costs recorded: ~870 KB of UI assets in the bundle; wasm builds add ~40 s to
  `bundle-obs`; two Dioxus versions in the lockfile is avoided (workspace-wide 0.7).

## 11. Decision log

- 2026-08-27 — Round 2: keep OBS's own resampler (push clips at native rate); the
  websocket smoke harness is the verification path for later rounds; a "now playing"
  overlay, if ever, is a separate video source (user decision).
- 2026-08-27 — Design space A chosen; macOS-first dev, Windows parity required for 1.0,
  Linux optional; both frontends ship for now; plugin owns its own db; `AudioSink` trait in
  `lupo-audio`, OBS impl in `lupo-obs`; own minimal `obs-sys` instead of the archived
  `obs-wrapper`; Properties dialog for MVP UI, browser dock as the candidate for UI reuse.

## 12. References

- OBS plugin docs: https://docs.obsproject.com/plugins
- Source reference (`obs_source_info`, audio output): https://docs.obsproject.com/reference-sources
- Properties reference: https://docs.obsproject.com/reference-properties
- Frontend API (docks): https://docs.obsproject.com/reference-frontend-api
- Official plugin template (CMake; useful for bundle layout and CI shape):
  https://github.com/obsproject/obs-plugintemplate
- Twitch Device Code flow: https://dev.twitch.tv/docs/authentication/getting-tokens-oauth/#device-code-grant-flow

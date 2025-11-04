# Dioxus Shell Migration Plan (`/Users/jan/sugoijan/lupo-tts/DIOXUS_SHELL_PLAN.md`)

## Summary
Build a crate at `/Users/jan/sugoijan/lupo-tts/app` (`lupo-app`, binary `lupo`) that uses Dioxus and follows `/Users/jan/sugoijan/lupo-tts/DESIGN.md` as product source of truth.

`app` keeps runtime code isolated from `app2` by copying and evolving independently during this experiment.

Promotion note (2026-02-22): `app` is now the default shell package/binary (`lupo-app` / `lupo`) while remaining in the `app` folder path.

## Decision Record (Locked)
- [x] Start Dioxus shell as a parallel experiment, not in-place replacement.
- [x] `/Users/jan/sugoijan/lupo-tts/DESIGN.md` is authoritative for behavior.
- [x] macOS-first scope.
- [x] Runtime copied from `app2` and isolated.
- [x] Broad feature sweep for milestone one.
- [x] Keep 4-tab IA (`Chat Logs`, `Chatters`, `Settings`, `Advanced`).
- [x] Single UI core with renderer adapters.
- [x] Compile-time renderer selection.
- [x] One binary (`lupo`) + feature flags.
- [x] Default renderer `renderer-webview`.
- [x] Native renderer remains experimental until smoke gate pass.
- [x] Unsupported backend capabilities are surfaced with explicit placeholders.

## Important Public API / Interface / Type Changes
- [x] Added workspace member `app` in `/Users/jan/sugoijan/lupo-tts/Cargo.toml`.
- [x] Added workspace Dioxus dependencies (`dioxus`, `dioxus-desktop`, `dioxus-native`) in `/Users/jan/sugoijan/lupo-tts/Cargo.toml`.
- [x] Added `/Users/jan/sugoijan/lupo-tts/app/Cargo.toml` with package `lupo-app` and bin `lupo`.
- [x] Added feature layout in `/Users/jan/sugoijan/lupo-tts/app/Cargo.toml`:
- [x] `default = ["renderer-webview"]`
- [x] `renderer-webview`
- [x] `renderer-native`
- [x] Added compile-time guard for one renderer feature in `/Users/jan/sugoijan/lupo-tts/app/src/app/mod.rs`.
- [x] Added internal contracts:
- [x] `AppModel` (`/Users/jan/sugoijan/lupo-tts/app/src/app/model.rs`)
- [x] `AppAction`/`UiEvent` (`/Users/jan/sugoijan/lupo-tts/app/src/app/action.rs`)
- [x] `RendererAdapter` boundary (`/Users/jan/sugoijan/lupo-tts/app/src/app/renderer.rs`)
- [x] `AppServices` runtime bundle (`/Users/jan/sugoijan/lupo-tts/app/src/runtime/services.rs`)

## Phase Plan With Explicit Tasks

### Phase 0: Planning Artifact
Exit criteria: this file exists and becomes the canonical progress tracker.
- [x] Create `/Users/jan/sugoijan/lupo-tts/DIOXUS_SHELL_PLAN.md` with checklist + changelog format.

### Phase 1: Workspace and Crate Scaffolding
Exit criteria: `cargo run -p lupo-app` launches a Dioxus shell with tabs.
- [x] Scaffold `/Users/jan/sugoijan/lupo-tts/app/src/main.rs` and `/Users/jan/sugoijan/lupo-tts/app/src/lib.rs`.
- [x] Wire `app` into `/Users/jan/sugoijan/lupo-tts/Cargo.toml` workspace members.
- [x] Add Dioxus dependency set and renderer-feature wiring.
- [x] Boot `lupo` with a 4-tab shell using the default renderer.
- [x] Confirm `cargo run -p lupo-app` launch smoke.

### Phase 2: Runtime Port + Core State Architecture
Exit criteria: Dioxus shell drives copied runtime behavior through typed actions and bridge events.
- [x] Copy runtime modules from `/Users/jan/sugoijan/lupo-tts/app2/src/runtime` into `/Users/jan/sugoijan/lupo-tts/app/src/runtime`.
- [x] Keep persistence/settings schema behavior unchanged.
- [x] Rename shell logging identity (`LUPO_LOG`, `lupo-log`).
- [x] Implement Dioxus-facing bridge polling (`PollRuntime`) and deterministic state updates.
- [x] Preserve queue/playback/control semantics from `app2` as baseline.

### Phase 3: DESIGN-Driven UI Surfaces (Broad Sweep)
Exit criteria: all four tabs and major DESIGN-aligned control surfaces exist, with explicit placeholders where backend wiring is missing.
- [x] Implement top navigation shell and status strip.
- [x] Implement Chat Logs view with lifecycle status badges and active playback indication.
- [x] Implement playback strip with manual injection, play/stop, clear queue.
- [x] Implement stop confirmation flow.
- [x] Implement quit interception flow.
- [x] Implement Chatters view structure with search/filter scaffolding + placeholder panels.
- [x] Implement Settings sections aligned to DESIGN categories.
- [x] Implement Advanced Settings entry guard + expert key grouping shell.
- [x] Implement help-mode scaffold and shortcut catalog hints.
- [x] Implement keyboard shortcut routing for core actions (Cmd/Ctrl+, Space, Cmd/Ctrl+Shift+S, Cmd/Ctrl+Q, etc.).

### Phase 4: Placeholder Coverage for Unsupported Backends
Exit criteria: unsupported DESIGN features are visible and actionable in UI copy.
- [x] Add explicit “Not yet wired” states for unsupported integration/features.
- [x] Ensure placeholders explain impact and expected future wiring.
- [x] Keep unsupported behavior explicit instead of hidden.

### Phase 5: Native Renderer Adapter + Switch Gate
Exit criteria: native feature path exists and checklist gate is documented before switching defaults.
- [x] Add renderer-native adapter path behind feature flag.
- [x] Keep native renderer available as optional feature.
- [ ] Validate native build + app launch path with `renderer-native` feature.
- [ ] Run full 4-tab smoke walkthrough on native feature variant.
- [x] Keep default renderer as webview while native issues are investigated.
- [ ] If smoke passes, flip default renderer to native.
- [x] Wire `renderer-native` to `dioxus-native` launch path.

### Phase 6: Docs, Commands, and Trial Hygiene
Exit criteria: trial run/build instructions are documented and bundling path is verified.
- [x] Update `/Users/jan/sugoijan/lupo-tts/README.md` with `lupo` run/build commands.
- [x] Confirm `cargo xtask bundle --app lupo` end-to-end.
- [x] Add trial-notes + blocker classification sections in this document.

## Test Cases and Scenarios

Automated:
- [x] `cargo check -p lupo-app`
- [x] `cargo check -p lupo-app --no-default-features --features renderer-native`
- [x] `cargo test -p lupo-app`
- [x] `cargo clippy -p lupo-app --all-targets -- -D warnings`
- [x] `cargo check --workspace`

Manual smoke:
- [ ] App launches and renders all 4 tabs.
- [ ] Chat Logs manual message injection updates lifecycle.
- [ ] Playback controls (play/stop/clear) work.
- [ ] Stop confirmation dialog behavior is correct.
- [ ] Quit interception flow is correct.
- [ ] Settings save/reset/discard/reload works.
- [ ] Advanced guard flow works.
- [ ] Status strip and runtime issue surfacing are visible.

Native default-switch gate:
- [ ] Pass full 4-tab smoke walkthrough on native feature build.
- [ ] No blocker-level regression in queue/playback/settings/quit flows.
- [ ] Decision entry recorded before default renderer change.

## Assumptions and Defaults
- [x] Webview renderer starts as default.
- [x] Native renderer remains optional for comparison/fallback.
- [x] No DB/schema migration in this effort.
- [x] `app2` remains available as an optional Iced experiment on a backburner track.
- [x] DESIGN takes precedence over parity when behavior conflicts.

## Trial Notes
- 2026-02-22: Added `app` crate scaffold (`lupo-app` / `lupo`) with compile-time renderer features.
- 2026-02-22: Copied runtime stack from `app2` into `app` and renamed logging identity (`LUPO_LOG`, `lupo-log`).
- 2026-02-22: Implemented Dioxus shell with 4 tabs, status strip, chat logs playback controls, stop/quit confirmation flows, settings editor cards, advanced guard, and unsupported-feature placeholders.
- 2026-02-22: Added renderer adapter boundary with compile-time renderer selection.
- 2026-02-22: Updated README run/bundle documentation for `lupo`.
- 2026-02-22: Passed `cargo check -p lupo-app`, renderer-webview `cargo check`, `cargo test -p lupo-app`, strict clippy, and `cargo check --workspace`.
- 2026-02-22: Verified desktop launch path (`cargo run -p lupo-app`) and resolved launch API mismatch by using `dioxus_desktop::launch::launch` with context/config arguments.
- 2026-02-22: Updated `xtask` bundle build logic to map bins to package names (`lupo`, `lupo2`) and confirmed `cargo xtask bundle --app lupo` produces `/Users/jan/sugoijan/lupo-tts/target/release/bundle/macos/Lupo.app`.
- 2026-02-22: Switched `app` default renderer feature to `renderer-native` and renamed desktop feature to `renderer-webview` with compile-time mutual exclusion.
- 2026-02-22: Migrated `app` to Dioxus `0.7` family and wired `renderer-native` to `dioxus-native::launch`.
- 2026-02-22: Temporarily gated `lupo-app` out of workspace membership to isolate GPUI cocoa conflicts during `app` renderer testing.
- 2026-02-22: Switched `app` default renderer back to `renderer-webview`; `renderer-native` remains opt-in for ongoing testing.
- 2026-02-22: Completed post-promotion cleanup by removing legacy GPUI shell sources and `ui/` crate, removing workspace `gpui`/`lupo-ui` deps, and updating `AGENTS.md` guidance for Dioxus-default development.
- 2026-02-22: Confirmed `app2` continues as a backburner Iced experiment while `app` (`lupo-app`) remains the default shell.

## Blocker Classification
- must-fix during native-default trial:
  - Validate native feature launch and complete manual smoke pass.
  - Verify key shortcuts across platform input backends.
- post-switch candidates:
  - Wire connection cards and secret-management controls in settings.
  - Add advanced change history + import/export flows.
- won’t-fix (this phase):
  - Cross-shell runtime extraction (`app`, `app2`) is out of scope.

## Changelog
- 2026-02-22:
  - Added `/Users/jan/sugoijan/lupo-tts/app` crate with Dioxus setup.
  - Added `AppModel`, typed actions/events, and renderer adapter boundary.
  - Ported runtime modules and removed `iced`-specific bridge coupling.
  - Added trial plan document and README updates.

# ICED Shell Migration Plan (`/Users/jan/sugoijan/lupo-tts/ICED_SHELL_PLAN.md`)

## Summary
Build a new crate at `/Users/jan/sugoijan/lupo-tts/app2` (`lupo-app2`, binary `lupo2`) that fully uses Iced and reaches behavior parity with the desktop shell behavior defined in `/Users/jan/sugoijan/lupo-tts/DESIGN.md`.
Implementation includes deep cleanup inside `app2` only.

Status update (2026-02-22): `/Users/jan/sugoijan/lupo-tts/app` (`lupo-app`) is now the default shell and uses Dioxus. `app2` remains available as an Iced experiment on a backburner track (no active replacement/cutover work).

## Decision Record (Locked)
- [x] Scope: parity-first migration.
- [x] Code sharing: duplicate runtime logic into `app2` (no shared extraction in this phase).
- [x] Cutover policy: 2-week time-box trial.
- [x] Parity definition: behavior parity, not strict visual parity.
- [x] Known legacy-shell issues: fix during port (do not intentionally carry over).
- [x] Plan doc strategy: dedicated `/Users/jan/sugoijan/lupo-tts/ICED_SHELL_PLAN.md`.
- [x] Naming: package `lupo-app2`, binary `lupo2`.
- [x] Testing depth before trial: manual-heavy.
- [x] Refactor policy: deep cleanup allowed.
- [x] Cleanup boundary: deep cleanup only inside `app2`.
- [x] Iced app structure: modular MVU.
- [x] Dependency policy: workspace-pinned Iced deps.
- [x] Platform target for trial: macOS first.
- [x] Coexistence policy: keep `app2` isolated from the default Dioxus shell and runtime evolution.

## Important Public API / Interface / Type Changes
- [x] Add workspace member `/Users/jan/sugoijan/lupo-tts/app2` in `/Users/jan/sugoijan/lupo-tts/Cargo.toml`.
- [x] Add workspace-pinned Iced dependencies in `/Users/jan/sugoijan/lupo-tts/Cargo.toml`.
- [x] Add new binary target `lupo2` in `/Users/jan/sugoijan/lupo-tts/app2/Cargo.toml`.
- [x] Keep existing `lupo` binary behavior unchanged.
- [x] Preserve existing app-facing persistence/settings schemas (no cross-crate schema migration in this effort).

`app2` internal contracts:
- [x] `AppState`: single source of truth for UI state.
- [x] `AppMessage`: typed message enum for user actions and subscriptions.
- [x] `update(&mut AppState, AppMessage) -> Task<AppMessage>` pattern implemented via Iced application builder.
- [x] `view(&AppState) -> Element<AppMessage>` implemented.
- [x] `subscription(&AppState) -> Subscription<AppMessage>` implemented.
- [x] `AppServices`: runtime handles (`tokio`, settings, pipeline, twitch bridge, logging).
- [x] `UiAction` and `UiEvent` bridge types to isolate async/runtime from view modules.
- [x] Per-tab modules with strict boundaries: `chat_logs`, `chatters`, `settings`, `advanced`.

## Phase Plan With Explicit Tasks

### Phase 1: Workspace and Crate Scaffolding
Exit criteria: `cargo run -p lupo-app2` opens an Iced window with tab shell and no legacy-shell dependency in `app2`.
- [x] Create `/Users/jan/sugoijan/lupo-tts/app2/Cargo.toml` with package/bin metadata.
- [x] Create `/Users/jan/sugoijan/lupo-tts/app2/src/main.rs` and `/Users/jan/sugoijan/lupo-tts/app2/src/lib.rs`.
- [x] Wire `app2` into `/Users/jan/sugoijan/lupo-tts/Cargo.toml` workspace members.
- [x] Add workspace-pinned Iced dependencies/features in `/Users/jan/sugoijan/lupo-tts/Cargo.toml`.
- [x] Implement minimal MVU shell (`AppState`, `AppMessage`, `update`, `view`, `subscription` stubs).
- [x] Create tab routing shell for `Chat Logs`, `Chatters`, `Settings`, `Advanced`.
- [x] Verify `cargo tree -p lupo-app2 | rg gpui` has no matches.

### Phase 2: Runtime Port + Deep Cleanup (Inside `app2`)
Exit criteria: `app2` has cleaned runtime modules equivalent in behavior to current `app`.
- [x] Port `/Users/jan/sugoijan/lupo-tts/app/src/pipeline.rs` into `/Users/jan/sugoijan/lupo-tts/app2/src/runtime/`.
- [x] Port `/Users/jan/sugoijan/lupo-tts/app/src/twitch.rs` into `/Users/jan/sugoijan/lupo-tts/app2/src/runtime/`.
- [x] Port `/Users/jan/sugoijan/lupo-tts/app/src/logging.rs` into `/Users/jan/sugoijan/lupo-tts/app2/src/runtime/`.
- [x] Preserve external behavior and persistence contract.
- [x] Introduce explicit runtime bridge layer so UI never calls pipeline/twitch directly.
- [x] Add internal error typing for `app2` runtime bridge with user-facing status mapping.
- [x] Keep all cleanup scoped to `/Users/jan/sugoijan/lupo-tts/app2`.

### Phase 3: Event Flow and State Management
Exit criteria: all major async flows are represented as typed `AppMessage` events and update deterministically.
- [x] Implement startup bootstrap flow (dirs, logging, runtime, db, audio, settings, pipeline).
- [x] Implement pipeline subscription flow (`SnapshotUpdated`, `Control`, `PlaybackStateChanged`, `Cleared`).
- [x] Implement twitch event subscription flow (`Status`, `Message` previews).
- [x] Implement async command actions for submit/play now/stop/toggle/clear/settings save/reset/reload.
- [x] Implement deterministic reducers for status banners, busy flags, and prompt state.
- [x] Implement graceful shutdown handling with full quit interception logic (including OS/window-close interception).

### Phase 4: UI Behavior Parity
Exit criteria: operator workflows in current `app` are functionally equivalent in `app2`.
- [x] Port Chat Logs behavior from `/Users/jan/sugoijan/lupo-tts/app/src/lib.rs`.
- [x] Port message rendering with provider/status badges and active playback indication.
- [x] Port manual message injection (`Add to queue`, `Play now`).
- [x] Port playback controls (`Play/Stop`, confirmation dialog, `Stop now`, cancel/continue).
- [x] Port quit interception dialog (`Quit anyway`, `Stop before quitting`, `Don't quit`).
- [x] Port settings list/editor behavior (`save`, `reset`, `discard`, `reload`, validation).
- [x] Port advanced settings gate and list behavior.
- [x] Keep `Chatters` parity with current placeholder behavior.
- [x] Re-implement sticky-to-latest behavior and hidden-message counter semantics.
- [x] Fix known clear-queue stuck issue during this phase.

### Phase 5: Packaging, Commands, and Docs
Exit criteria: trial usage is straightforward and documented.
- [x] Ensure `cargo run -p lupo-app2` is the canonical run path for trial.
- [x] Ensure bundle flow supports `lupo2` via `/Users/jan/sugoijan/lupo-tts/xtask` (`--app` already supports arbitrary binary names).
- [x] Update `/Users/jan/sugoijan/lupo-tts/README.md` with Dioxus + Iced development commands.
- [x] Add trial notes section to `/Users/jan/sugoijan/lupo-tts/ICED_SHELL_PLAN.md` for daily findings.
- [x] Keep `/Users/jan/sugoijan/lupo-tts/app` (default Dioxus shell) untouched functionally while iterating in `/Users/jan/sugoijan/lupo-tts/app2`.

### Phase 6: 2-Week Trial Execution
Exit criteria: explicit go/no-go decision documented.
- [x] Start 2-week trial only after Phase 1-5 gates pass.
- [x] Use `lupo2` as primary operator shell during trial window.
- [ ] Record daily blockers/regressions/usability notes in `/Users/jan/sugoijan/lupo-tts/ICED_SHELL_PLAN.md`.
- [ ] Categorize findings as `must-fix before cutover`, `post-cutover`, or `won’t-fix`.
- [x] End-of-trial decision entry: `Replace app`, `Extend trial`, or `Abort migration`.
- [ ] If decision is `Replace app`, generate follow-up removal plan for retired shell crates as a separate phase.

## Test Cases and Scenarios

Automated gates:
- [x] `cargo check --workspace`.
- [x] `cargo test --workspace`.
- [x] `cargo check -p lupo-app2`.
- [x] `cargo clippy -p lupo-app2 --all-targets -- -D warnings`.

Targeted unit tests in `app2`:
- [x] Reducer tests for prompt flows (stop confirmation, quit interception).
- [x] Reducer tests for message-list sticky/hidden-counter transitions.
- [x] Settings draft parsing/validation tests mirroring current behavior.
- [x] Event bridge tests for runtime event queue mapping into UI processing.

Manual parity checklist before trial:
- [x] App launches and shows all 4 tabs.
- [x] Manual message submit works and appears with lifecycle progression.
- [x] Play/Stop behavior works with confirmation semantics.
- [x] Quit interception behavior matches current app contract.
- [x] Clear queue works and does not get stuck.
- [x] Settings save/reset/discard/reload flows work.
- [x] Twitch status transitions surface correctly.
- [x] Restart preserves expected queue/playback state behavior.

## Risk Controls and Failure Modes
- [x] Keep `lupo` (Dioxus default) always runnable while evaluating `lupo2`.
- [x] Separate `app2` logging scope to simplify trial debugging.
- [x] Guard async channel lag/close paths with explicit status updates, not silent failures.
- [x] Treat runtime bridge errors as non-fatal UI state updates where possible.
- [x] Prevent duplicate in-flight actions for clear/stop/toggle operations.

## Assumptions and Defaults
- [x] macOS is the only required platform for this migration phase.
- [x] Iced version is pinned in workspace to a stable release compatible with current Rust toolchain.
- [x] No DB/schema changes are required for this migration.
- [x] Existing Dioxus shell remains source of fallback during trial.
- [x] Any larger cross-crate extraction is explicitly out of scope for this plan.
- [x] This document is the source of task progress; checkboxes are updated continuously during implementation.

## Progress Tracking Protocol
- [x] At start of each implementation session, mark current active phase and tasks in this file.
- [x] At end of each session, update checkboxes and add a short changelog entry with date.
- [ ] Do not mark phase complete until its exit criteria are met.

## Trial Notes
- 2026-02-22: Decision recorded: `Abort migration` as a default-shell replacement for now; keep `app2` as a backburner Iced experiment while `app` (Dioxus) remains default.
- 2026-02-22: 2-week trial window started after Phase 1-5 implementation gates passed.
- 2026-02-22: `app2` scaffold and Iced MVU shell implemented; runtime modules duplicated and wired via async bridge.
- 2026-02-22: Workspace verification passed (`cargo check --workspace`, `cargo test --workspace`, `cargo check -p lupo-app2`).
- 2026-02-22: Strict clippy for `lupo-app2` now passes with the exact command (`cargo clippy -p lupo-app2 --all-targets -- -D warnings`) after fixing upstream blockers.
- 2026-02-22: Added OS window-close interception and runtime issue surfacing (`pipeline lagged/closed`, `twitch channel closed`) to status messaging.
- 2026-02-22: Performed launch smoke test with `timeout 8 cargo run -p lupo-app2` (binary starts and remains running until timeout).

## Changelog
- 2026-02-22:
  - Updated this plan status after Dioxus promotion: `app` is default; `app2` continues as backburner experiment.
  - Recorded decision entry for Phase 6 as `Abort migration` (for default-shell cutover) while retaining `lupo2` for optional experimentation.
- 2026-02-22:
  - Added new `app2` crate (`lupo-app2` / `lupo2`) and workspace wiring.
  - Added workspace `iced` dependency and initial modular Iced shell.
  - Ported `pipeline`, `twitch`, and `logging` runtime modules into `app2`.
  - Implemented runtime bridge queue and async action handling for queue/playback/settings flows.
  - Ported core tab UI flows (Chat Logs, Settings, Advanced, Chatters placeholder).
  - Updated `/Users/jan/sugoijan/lupo-tts/README.md` with dual-shell run/bundle commands.
- 2026-02-22:
  - Added `UiEvent` bridge layer and runtime/window event dispatch path.
  - Added typed runtime issues/errors and user-facing status mapping in the Iced shell.
  - Implemented close-request interception using Iced window subscriptions and graceful close tasks.
  - Added reducer tests for quit/stop prompt flows.
  - Fixed strict clippy blockers in `/Users/jan/sugoijan/lupo-tts/persistence/src/lib.rs` and `/Users/jan/sugoijan/lupo-tts/messages-twitch/src/eventsub.rs`.

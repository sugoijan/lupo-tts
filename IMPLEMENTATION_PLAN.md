# Lupo Concrete Implementation Plan Document (Current State + Phased Runs)

## Summary

- Create a dedicated implementation plan at `/Users/jan/sugoijan/lupo-tts/IMPLEMENTATION_PLAN.md`.
- Keep `/Users/jan/sugoijan/lupo-tts/DESIGN.md` as product/UX source of truth and link to the implementation plan.
- Seed the new plan with a status snapshot from a code audit completed on **February 21, 2026** (`cargo check --workspace` and `cargo test --workspace` both pass).
- Execute implementation in **6 independent phases**, each with measurable exit criteria and full context so no phase depends on undocumented tribal knowledge.

## Current Implementation Snapshot

- [x] Dioxus desktop shell (`app`, `lupo-app`) with tabs for `Chat Logs`, `Chatters`, `Settings`, and `Advanced Settings` exists.
- [x] Chat log UI renders persisted message lifecycle status badges.
- [x] Manual message injection exists and enters the same pipeline.
- [x] Message pipeline exists with transformation, synthesis, and playback workers.
- [x] OpenAI TTS is integrated into runtime pipeline synthesis.
- [x] Audio playback engine is integrated and used by pipeline playback.
- [x] Message persistence stores snapshots, lifecycle history, and audio artifacts.
- [x] Startup bootstraps existing persisted messages.
- [x] Twitch OAuth device flow and token refresh paths are implemented.
- [x] Twitch EventSub chat ingestion is implemented.
- [x] Emote resolver stack exists (Twitch, BTTV, 7TV, FFZ, emoji).
- [x] Settings schema/registry and CLI settings management are implemented.
- [x] Secret storage abstraction over embedded DB is implemented.
- [x] Chatters view functionality is implemented with searchable/filterable overrides and metadata.
- [x] Settings UI supports per-key draft/edit/save/reset/discard, with connection cards, secret controls, and advanced utilities implemented.
- [x] Filtering stage and command acceptance logic are implemented in runtime pipeline.
- [x] Queue overflow prediction/back-pressure policies are implemented.
- [x] Retry/unskip operator flows are implemented.
- [ ] Playback UX parity remains partial (VU meter strip remains a placeholder).
- [x] Multi-provider runtime strategy is implemented in app pipeline.
- [x] Offline-first runtime behavior is implemented (System/Parler/Ultra-light paths with ordered fallback).
- [x] YouTube Live Chat integration is implemented and normalized into canonical message schema.
- [x] Help mode, onboarding aids, and shortcut catalog are implemented.
- [ ] Reliability requirements are incomplete (reconnect backoff loop, safe mode, missed-message recovery).

## Phase 1: Canonical Message Contract + Filtering

Goal: Bring message schema and lifecycle behavior into alignment with design requirements before adding more UI/control complexity.

Evaluation criteria:

- Every incoming message has required canonical metadata fields.
- Messages deterministically end up in `Filtered` or `Skipped` (with reason) before transformation.
- Command parsing and command-acceptance policy are enforced via settings.

Checklist:

- [x] Extend persisted message schema to include provider message ID, channel ID, canonical user ID, source timestamp, delivery kind, payment payload, and metadata map.
- [x] Use clean schema rollout for phase-1 (no backward compatibility); local dev DB reset is acceptable.
- [x] Implement explicit filtering worker/stage in runtime pipeline.
- [x] Implement `message_filtering` and `command_acceptance` settings enforcement in filtering stage.
- [x] Implement command prefix handling (`!lupo` default) with control-event emission and source-message suppression rules.
- [x] Implement moderator deletion handling and operator override precedence rules.
- [x] Persist skip/error reason codes for all terminal non-play states.
- [x] Add unit/integration tests for filtering, command parsing, and state transitions.

Implementation slices (ready to code):

1) Persistence contract + versioned compatibility

- Files: `persistence/src/lib.rs`, `runtime/src/pipeline.rs`
- Add canonical header fields and supporting types:
  - `provider_message_id: Option<String>`
  - `channel_id: Option<String>`
  - `user_id: Option<String>`
  - `source_timestamp: Option<OffsetDateTime>`
  - `delivery_kind: DeliveryKind`
  - `payment: Option<PaymentInfo>`
  - `metadata: BTreeMap<String, serde_json::Value>`
- Bump snapshot format to the phase-1 canonical shape without v1/v0 hydration paths.
- Document/reset local dev database when applying the schema change.
- Extend skip reason coverage with stable machine codes for filtering outcomes.

2) Ingestion enrichment to feed canonical schema

- Files: `messages-twitch/src/eventsub.rs`, `runtime/src/twitch.rs`, `runtime/src/pipeline.rs`
- Extend Twitch chat message surface with canonical IDs/timestamps needed by filtering:
  - chatter user ID, provider message ID, broadcaster/channel ID, source timestamp when available.
- Populate `NewMessage` canonical fields from Twitch payloads.
- Populate command/filter-relevant metadata flags (role/bot markers) from provider payload.
- Keep local/manual injection compatible by supplying local defaults.

3) Explicit filtering stage in pipeline core

- Files: `runtime/src/pipeline.rs`
- Add filtering queue, worker(s), stage results, and scheduling branch:
  - `Arrived -> Filtering -> (Filtered | Skipped)`
  - `Filtered -> Transformation` only.
- Ensure idempotent scheduling on bootstrap/restart.
- Persist stage spans and transition notes for filter pass/fail reasons.

4) Settings-backed filter and command policy evaluation

- Files: `runtime/src/pipeline.rs`, `configuration/src/lib.rs` (read-only unless schema shape changes)
- Parse `message_filtering` and `command_acceptance` setting values into typed runtime policy.
- Enforce:
  - general ignore prefix rule
  - dedicated command prefix rule
  - provider bot suppression
  - paid-message suppression toggle
  - command acceptance roles (`nobody`, `moderators`, `channel_owner`)
- Suppress command source messages from TTS path while emitting control events.

5) Moderator deletion handling + precedence rule

- Files: `messages-twitch/src/eventsub.rs`, `runtime/src/twitch.rs`, `runtime/src/pipeline.rs`
- Subscribe/parse moderator deletion events and route to pipeline by provider message ID.
- Transition matching non-played messages to `Skipped(ModeratorDeleted)` with reason code.
- Ignore moderator delete for already played messages.
- Respect existing manual override precedence when override markers are present.

Phase-1 test plan (must ship with phase):

- `persistence`: canonical snapshot encode/decode round-trip using the new schema only.
- `pipeline`: arrived message runs filter first; transformation never starts before `Filtered`.
- `pipeline`: filtering matrix for prefix/command/bot/paid rules.
- `pipeline`: command acceptance matrix for `nobody`, `moderators`, `channel_owner`.
- `pipeline`: command messages emit control event and source message is suppressed.
- `pipeline+twitch`: moderator delete event maps to `Skipped(ModeratorDeleted)` unless already `Played`.

## Phase 2: Queue Policy + Playback Control Semantics

Goal: Make queue behavior and operator controls match the design contract.

Evaluation criteria:

- Operators can reliably play/stop/skip/retry/unskip with expected lifecycle transitions.
- Queue sizing and overflow handling work with predicted and actual duration checks.
- Stop/quit confirmation logic matches documented behavior.

Checklist:

- [x] Add queue scheduler with predicted duration accounting and overflow strategies.
- [x] Add pipeline commands for skip, retry, unskip, force-play, and pin-next.
- [x] Implement stop-confirmation UX flow (`Continue playback`, `Stop now`, `Cancel`).
- [x] Implement quit interception flow for queued/playing messages.
- [x] Define chat-log ordering + auto-scroll behavior (`newest at bottom`, sticky-to-latest unless operator manually scrolls up).
- [x] Preserve active playback row focus/visibility and hidden-message counter behavior.
- [x] Persist and restore queue position and playback toggle state.
- [ ] Add tests for queue ordering, override precedence, and overflow paths (`Deferred`: see `/Users/jan/sugoijan/lupo-tts/SKIPPED_TASKS.md`).

Phase-2 kickoff notes (code audit on **February 21, 2026**):

- Playback currently marks `Played` right after `audio.play_wav(...)` dispatch; this does not wait for actual audio drain and causes lifecycle/UX drift.
- Pipeline has chat command filtering/control events but no operator action command path (`skip`, `retry`, `unskip`, `pin_next`, `force_play`, `stop_now`).
- Chat log currently inserts newest entries at the top; design target is `newest at bottom` with sticky-to-latest semantics.
- Quit path currently exits immediately without queued/playing interception.

Phase-2 implementation slices (ready to code):

1) Operator actions + playback policy state in pipeline core

- Files: `runtime/src/pipeline.rs`, `persistence/src/lib.rs` (only if state model additions are needed)
- Add new pipeline commands and public entry points:
  - `skip_message(message_id)`
  - `retry_message(message_id)`
  - `unskip_message(message_id)`
  - `pin_next(message_id)`
  - `force_play(message_id)`
  - `set_playback_enabled(enabled)` / `stop_now()`
- Add deterministic precedence handling in scheduler:
  - operator overrides > moderator commands > default policy.
- Introduce explicit runtime playback toggle state and expose it to UI via pipeline events.

2) Queue scheduler + overflow and prediction guards

- Files: `runtime/src/pipeline.rs`, `configuration/src/lib.rs` (if additional advanced keys are required)
- Add queue model that tracks:
  - ordered candidate set by priority + override markers
  - predicted queue duration budget
  - actual synthesized duration budget
- Add transformation-time predicted duration estimator and guard:
  - skip/defer with `PredictedQueueOverflow` when over threshold.
- Add post-synthesis guard using actual duration:
  - skip with `QueueOverflow` when predicted guard underestimates.

3) Playback stage semantics + interruption correctness

- Files: `runtime/src/pipeline.rs`, `audio/src/lib.rs` (only if cancel/flush API extension is required)
- Transition to `Played` only after actual playback drain.
- Add stop/skip-current behavior that interrupts active playback and keeps remaining queue.
- Ensure `stop_now` is idempotent and race-safe with concurrent stage results.
- Persist transition notes and reason codes for operator-driven stops/skips.

4) Chat-log ordering, sticky scroll, active-row focus, hidden counter

- Files: `app/src/app/model.rs`, `app/src/app/state.rs`, `app/src/app/ui.rs`
- Switch list model to bottom-appended chronological rendering.
- Add sticky-to-latest behavior unless operator manually scrolls up.
- Keep active playing row visible/focused while playback is active.
- Track and render hidden-message counter when viewport is not at latest.

5) Stop confirmation + quit interception

- Files: `app/src/app/model.rs`, `app/src/app/state.rs`, `app/src/app/ui.rs`
- Add play/stop control state bound to pipeline playback toggle.
- Stop flow:
  - open confirmation with `Continue playback`, `Stop now`, `Cancel`.
  - `Stop now` calls pipeline `stop_now()` without clearing queue.
- Quit flow:
  - if any message is `Queued`/`Playing`, show `Quit anyway`, `Stop before quitting`, `Don't quit`.
  - `Stop before quitting` chains through the same stop-confirm path.

6) Session persistence for phase-2 controls

- Files: `persistence/src/lib.rs`, `runtime/src/pipeline.rs`, `app/src/app/model.rs`
- Persist and restore:
  - playback toggle state,
  - queue cursor / next-candidate ordering hints needed for deterministic resume.
- Ensure bootstrap path rehydrates scheduler state before replaying work.

Phase-2 test plan (must ship with phase):

- `pipeline`: operator command transitions for skip/retry/unskip/pin-next/force-play/stop-now.
- `pipeline`: ordering/precedence matrix (operator override > moderator > default).
- `pipeline`: predicted overflow and post-synthesis overflow guard tests.
- `pipeline`: playback completion occurs after drain, not dispatch.
- `app`: chat-log bottom-order + sticky-scroll + hidden counter behavior.
- `app`: stop confirmation flow choices and resulting pipeline calls.
- `app`: quit interception choices and stop-before-quit chaining.
- `bootstrap`: queue cursor/playback toggle restore after restart.

## Phase 3: Settings and Advanced Settings as Operational Controls

Goal: Turn settings from mostly informational into fully functional runtime controls.

Evaluation criteria:

- All settings sections from design are editable and validated in app UI.
- Runtime behavior updates from settings without restart where feasible.
- Advanced settings has explicit safety and traceability controls.

Checklist:

- [x] Implement editable settings panels for schema-backed domains (filtering, command acceptance, text transformation, TTS strategy, offline strategy, voice strategy, and audio output).
- [x] Add connection cards with state, reconnect, and revoke actions.
- [x] Add secure UI flows for provider secrets (set/update/revoke OpenAI token at minimum) so CLI is optional for normal operation.
- [x] Wire settings changes to live pipeline/connector behavior, including connector restart/reconnect hooks.
- [x] Implement advanced settings per-open confirmation guard.
- [x] Implement advanced-setting change log with timestamped history.
- [x] Implement advanced reset/export/import utilities in UI.
- [x] Add keyboard settings shortcut entry point (`Cmd+,`/`Ctrl+,`).
- [x] Add menu-level settings entry point.
- [x] Fix settings-tab overflow behavior so long content remains scrollable and controls stay interactive.
- [x] Add tests for settings validation, persistence, and runtime application.

Phase-3 kickoff notes (code audit on **February 22, 2026**):

- Dioxus settings UI already has per-key editor state (`draft`, `dirty`, validation error) and save/reset/discard flows wired through `SettingsService`.
- Settings sections for filtering/command acceptance/transformation/TTS/offline/voice/audio are grouped and rendered in `app/src/app/ui.rs`.
- Connections panel is still placeholder-only and does not expose reconnect/revoke/auth controls yet.
- Advanced access is still a one-way in-session toggle (`show_advanced`) instead of per-open confirmation.
- `Cmd+,`/`Ctrl+,` shortcut now works in the active Dioxus renderer (global desktop shortcut hook + in-app key mapping fallback).
- `SettingsService` APIs required for advanced utilities already exist: `set`, `reset`, `reset_all`, `snapshot`, `apply_snapshot`.
- Secret helpers (`store_secret`, `load_secret_string`, `delete_secret`) exist, but no secure UI flow exposes them yet.
- QA found a Dioxus settings-tab regression where overflowing content can block scrolling and make Save/Reset/Discard controls non-interactive; this is now a phase entry blocker.

Phase-3 implementation slices (remaining work after Dioxus rework):

1) Stabilize settings-tab layout and control interactivity

- Files: `app/src/app/ui.rs`, `app/src/app/state.rs`
- Ensure `Settings` and `Advanced` content remains vertically scrollable with long forms and many cards.
- Ensure action buttons (`Save`, `Reset`, `Discard`) remain clickable under overflow and modal layering conditions.
- Keep sticky/top-level shell behavior unchanged while fixing local tab scroll containers.

2) Connection cards + secure secret controls

- Files: `app/src/app/action.rs`, `app/src/app/model.rs`, `app/src/app/ui.rs`, `runtime/src/twitch.rs`, `secrets/src/lib.rs`
- Add connection cards with state indicator, reconnect action, and revoke action.
- Add OpenAI key controls (set/update/revoke) in UI:
  - write via secure-store helpers,
  - mask token values in UI (never render plaintext),
  - expose last-updated metadata where available.
- Add Twitch credential revoke/reconnect wiring via existing secret/settings storage paths.

3) Live runtime behavior updates from settings

- Files: `app/src/app/model.rs`, `runtime/src/pipeline.rs`, `runtime/src/twitch.rs`
- On save, persist via `SettingsService` and immediately apply behavior where supported:
  - filtering and command acceptance (next message evaluation),
  - advanced scheduler limits (next budget check),
  - connector actions (reconnect/revoke/restart).
- Surface save/apply errors inline in settings UI with actionable operator feedback.

4) Advanced safety guard + change history

- Files: `app/src/app/action.rs`, `app/src/app/model.rs`, `app/src/app/state.rs`, `app/src/app/ui.rs`
- Require explicit confirmation every time `Advanced Settings` is opened (tab click, shortcut navigation, or future menu entry).
- Record advanced-setting changes with timestamp, key, previous value, and new value.
- Keep advanced-only keys isolated from the standard settings tab.

5) Advanced import/export/reset utilities

- Files: `app/src/app/action.rs`, `app/src/app/model.rs`, `app/src/app/ui.rs`, `configuration/src/lib.rs`
- Add UI actions:
  - reset one advanced key,
  - reset all advanced keys,
  - export snapshot,
  - import snapshot with dry-run preview before apply.
- Route import/export through `SettingsService::snapshot` and `SettingsService::apply_snapshot`.

6) Settings entry points in renderer/menu

- Files: `app/src/app/renderer.rs`, `app/src/app/action.rs`, `app/src/app/model.rs`
- Keep existing keyboard/tab entry paths.
- Add menu-level settings entry point for renderers that support it.
- Ensure menu action routes to the same settings-tab activation path as keyboard shortcut handling.

Phase-3 test plan (must ship with phase):

- `configuration`: validation tests for type/enum mismatches on edited settings keys.
- `app`: settings and advanced tabs remain scrollable with overflowing content; `Save`/`Reset`/`Discard` controls remain interactive.
- `app`: settings edit/save/reset flows update persisted values and refresh the effective listing.
- `app`: advanced view requires per-open confirmation (not a one-time toggle).
- `app`: advanced import dry-run reports changes without persisting; apply mode persists changes.
- `app+twitch`: reconnect and revoke controls change visible connection state and persisted credentials.
- `app+secrets`: OpenAI token set/revoke path updates secret metadata and never exposes plaintext in render state.
- `pipeline+app`: changed filtering/command acceptance settings affect subsequent message decisions without app restart.

## Phase 4: Provider Matrix + Offline-First Runtime

Goal: Deliver runtime provider strategy with offline-first default and cloud fallback.

Evaluation criteria:

- App is usable offline out of the box.
- Runtime can select/fallback across configured providers.
- Provider status/expiry/quota surfaces in UI.

Checklist:

- [x] Introduce runtime TTS provider abstraction used by pipeline (instead of OpenAI-only path).
- [x] Integrate Parler provider into app runtime path.
- [x] Integrate macOS system TTS capture provider into app runtime path (platform-gated).
- [x] Implement lightweight fallback engine option for constrained environments.
- [x] Implement provider priority and fallback rules from settings.
- [x] Extend status strip to include provider readiness/error states beyond Twitch.
- [x] Implement YouTube Live Chat connector and normalization into shared schema.
- [x] Add tests for provider selection/fallback and offline operation.

## Phase 5: Chatters + Moderation UX + Help/Onboarding

Goal: Complete core operator workflow UX for live operation.

Evaluation criteria:

- Chatters tab is functional for lookup and per-user overrides.
- Chat log row actions and contextual moderation controls are usable.
- Help/onboarding and shortcuts are discoverable and functional.

Checklist:

- [x] Implement chatter directory with search/filter, roles/badges/counts, notes, and voice overrides.
- [x] Persist chatter overrides and attach them to synthesis decisions.
- [x] Implement row selection and context actions (copy, replay, skip, profile/context link).
- [x] Render message metadata fidelity (badges, color, emotes, source timestamp tooltip behavior).
- [x] Improve message-injection input UX (stable caret visibility/blink, fixed-height field, full-width layout behavior).
- [x] Implement first-run onboarding state and no-provider warning banner path.
- [x] Implement help menu, contextual help mode, and shortcut catalog.
- [x] Implement documented keyboard shortcuts for play/stop/skip/volume.

## Phase 6: Reliability, Recovery, and Release Hardening

Goal: Reach stable operational behavior with measurable reliability/performance.

Evaluation criteria:

- Connection and session recovery behavior match design.
- Crash-loop and safe-mode protections exist.
- Telemetry provides actionable latency/retry/queue-depth insight.

Checklist:

- [ ] Implement connector reconnection loop with exponential backoff.
- [ ] Implement session recovery for connections, queue state, and playback state.
- [ ] Implement missed-message recovery when provider APIs permit.
- [ ] Implement safe-mode startup after repeated crash loops.
- [ ] Implement structured telemetry for stage latency, retries, queue depth, and manual overrides.
- [ ] Expand automated tests for recovery, reconnect, and long-running queue behavior.
- [ ] Add release-readiness checklist and incident runbook references.

## Important API / Interface / Type Changes

- [x] Expand persisted `MessageHeader`/ingestion contract to canonical cross-provider schema fields.
- [x] Add explicit filtering and control-event interfaces between ingestion and transformation stages.
- [x] Add operator action command interface to pipeline (`skip`, `retry`, `unskip`, `pin_next`, `force_play`, `stop_now`).
- [x] Introduce runtime TTS provider trait and adapter registry for OpenAI/Parler/System providers.
- [ ] Add chatter override data model and persistence contract.
- [x] Add recovery state model for playback toggle, queue cursor, and connector state.
- [ ] Add telemetry event schema for transition/error/override observability.

## Test Cases and Scenarios (Track in Plan as Checkboxes)

- [ ] Message lifecycle golden-path transition test from ingestion to played.
- [ ] Filtering matrix test (prefix rules, command rules, bot suppression, operator override).
- [ ] Command acceptance role test (`nobody`, `moderators`, `channel_owner`).
- [ ] Queue overflow prediction vs actual-duration guard tests.
- [ ] Retry/unskip/error recovery transition tests.
- [x] Provider fallback tests (offline primary, cloud secondary, failover).
- [ ] Twitch token refresh and reconnect tests.
- [x] YouTube connector ingestion/normalization tests.
- [ ] UI tests for stop confirmation, quit interception, advanced-settings confirmation.
- [ ] Session restore tests after forced restart/crash simulation.

## Native Renderer Gate

Carried over from the retired `DIOXUS_SHELL_PLAN.md`. `lupo-app` defaults to `renderer-webview`; `renderer-native` (`dioxus-native`) is opt-in via `cargo run -p lupo-app --no-default-features --features renderer-native` and stays experimental until this gate passes.

- [x] `cargo check -p lupo-app --no-default-features --features renderer-native` compiles (verified 2026-08-27; emits dead-code warnings for webview-only menu constants in `app/src/app/action.rs`).
- [ ] Full 4-tab manual smoke walkthrough on the native build (launch, manual injection lifecycle, play/stop/clear, stop confirmation, quit interception, settings save/reset/discard/reload, advanced guard, status strip).
- [ ] Keyboard shortcuts verified across platform input backends.
- [ ] No blocker-level regression in queue/playback/settings/quit flows.
- [ ] Decision entry recorded here before flipping the default renderer feature.

## Assumptions and Defaults

- [x] Plan document location is `/Users/jan/sugoijan/lupo-tts/IMPLEMENTATION_PLAN.md`.
- [x] `DESIGN.md` remains the product design source; plan doc is implementation execution source.
- [x] Deferred/skipped implementation items are tracked in `/Users/jan/sugoijan/lupo-tts/SKIPPED_TASKS.md`.
- [x] Phase granularity is 6 independent runs.
- [x] Phase completion is evaluated against explicit criteria, not task count alone.
- [x] Each phase must update this plan document status (`- [ ]` to `- [x]`) before starting the next phase.
- [x] Planning-only turns may modify this document to add implementation-ready execution details.

# Skipped Tasks and Refactor Backlog

This document tracks implementation tasks that were intentionally deferred, why they were deferred, and the largest architectural changes needed to make them practical later.

Use this as review input after all implementation phases to plan targeted refactors.

## Conventions

- `Status`: `Deferred`, `Replaced`, or `Completed`.
- `Source`: phase/checklist item where the task originated.
- `Why skipped`: concrete reason it was not finished in that phase.
- `Largest architectural changes needed`: highest-impact refactors required before re-attempting.

## Deferred Items

### ST-P2-001: Deterministic queue ordering + precedence integration tests

- Status: Deferred
- Source: `IMPLEMENTATION_PLAN.md` Phase 2 checklist, "Add tests for queue ordering, override precedence, and overflow paths."
- Why skipped:
  - Queue decisions currently run inside an async runtime loop with live stage dispatch and shared mutable state.
  - Ordering behavior depends on real-time transition timing (`OffsetDateTime::now_utc`) and side effects from playback worker completion.
  - Existing unit tests cover helper logic, but not full scheduler evolution across mixed operator/moderator/default inputs.
- Largest architectural changes needed:
  - Extract queue decision logic into a pure scheduler module (state + command -> deterministic next action).
  - Introduce an injected clock trait so tests can control timestamps.
  - Add a deterministic pipeline harness that can drive stage results without real worker threads.

### ST-P2-002: End-to-end overflow tests (predicted vs actual duration guards)

- Status: Deferred
- Source: `IMPLEMENTATION_PLAN.md` test scenarios, "Queue overflow prediction vs actual-duration guard tests."
- Why skipped:
  - Current overflow checks are integrated into runtime scheduling and synthesis completion paths.
  - Coverage exists for utility functions, but not for multi-message budget progression across stage boundaries.
  - Tests need controlled synthesis/playback durations without depending on external TTS/audio behavior.
- Largest architectural changes needed:
  - Separate budget accounting into a dedicated component with deterministic inputs/outputs.
  - Define a synthetic stage-result test API to feed predicted and actual durations directly.
  - Add transition trace assertions for `PredictedQueueOverflow` vs `QueueOverflow` reason-code correctness.

### ST-P2-003: UI automation coverage for stop confirmation and quit interception

- Status: Deferred
- Source: `IMPLEMENTATION_PLAN.md` test scenarios, "UI tests for stop confirmation, quit interception, advanced-settings confirmation."
- Why skipped:
  - Shell view behavior is currently tested manually (Dioxus shell); there is no stable UI test harness for modal/prompt flows.
  - Quit interception depends on app-global wiring and window lifecycle callbacks that are not isolated for tests.
- Largest architectural changes needed:
  - Split UI behavior into a testable view-model/state reducer independent of rendering widgets.
  - Introduce action/intent dispatch interfaces that can be driven by tests without a real window runtime.
  - Add a small app-shell harness for window-close/quit lifecycle simulation.

### ST-P2-004: Restart/crash simulation tests for queue cursor and playback-toggle restore

- Status: Deferred
- Source: `IMPLEMENTATION_PLAN.md` test scenarios, "Session restore tests after forced restart/crash simulation."
- Why skipped:
  - Runtime state persistence is implemented, but tests do not yet simulate full process restart boundaries.
  - Startup currently combines runtime construction, service startup, and UI bootstrapping in one path.
- Largest architectural changes needed:
  - Introduce a bootstrap service layer with explicit `load_state -> initialize_pipeline -> resume` steps.
  - Add an integration harness that starts/stops services against a temporary DB and re-validates restored state.
  - Define a stable serialized runtime-state contract test fixture for backward-compatibility checks.

## Next Append Rule

For each future phase, append deferred items here instead of expanding ad-hoc notes in commit messages or PR text.

# Known Issues

## UI-QUEUE-001: `Clear queue` can get stuck

- First observed: February 21, 2026
- Area: `app` UI + pipeline coordination
- Status: Open
- Planned phase: Phase 2 (`Queue Policy + Playback Control Semantics`)

### Summary

Clicking `Clear queue` may leave the UI stuck in the `Clearing queue...` state while chat rows remain visible.

### Reproduction

1. Launch `lupo-app`.
2. Ensure chat rows are present in `Chat Logs`.
3. Click `Clear queue`.

### Expected

- Queue and visible rows are cleared.
- Status transitions to `Queue cleared.` (or equivalent success state).

### Actual

- Status remains `Clearing queue...`.
- Existing rows remain visible.

### Notes

- The issue is tracked for Phase 2 because it affects operator queue control semantics.

# Lupo TTS Application Design

## Document Scope

- Provide a shared reference for UI/UX design and software implementation;
- Define required behaviors, integrations, and constraints for the desktop application;
- Highlight open items that still need follow-up assets such as detailed user journeys and flow diagrams.

## Product Goal

- Enable operators (typically the channel owner) and trusted moderators to hear chat messages delivered in real time to support cases where visually reading messages is impractical;
- Support mixed-language conversations by allowing selectable TTS backends that handle multilingual content;
- Offer configurable policies so operators can manage which messages are spoken, which voices are used, and how playback behaves;
- Ensure third-party integrations bill at the provider's direct rates when used, and ship a local/offline TTS mode in the MVP so the application can operate without external service dependencies or outage risk.

## Primary Actors

- **Operator:** owns the active session, configures connections, and usually corresponds to the channel owner/streamer. The operator drives any desktop-only controls and can always perform moderator actions.
- **Moderator:** any chatter with moderation privileges on the connected platform, including the operator. Moderators issue command messages (e.g., `!lupo …`) from chat and never gain remote desktop access.

## Experience Overview

- **First-run state:** ship with the bundled offline TTS engine and `local` message injection enabled so playback is immediately usable. Surface a persistent warning banner when no external providers are connected and link directly to `Settings` to help operators add connections at their convenience.
- **Live operation:** display incoming chat, synthesize audio, and play messages through a controllable queue.
- **Session recovery:** resume the last known state after restarts or crashes, retaining filtered chats and active connections where possible.

## User Interface Structure

Ascii mock:

```
┌───────────────────────────────────────────────────────────────────────────────────────────┐
│ × _ ◻  Chat Logs | Chatters | Settings | [Advanced Settings | hidden ]                    │
│───────────────────────────────────────────────────────────────────────────────────────────│
│ Status strip: Twitch [ready]  YouTube [reconnecting]  OpenAI [ready]                      │
│ ┌───────────────────────────────────────────────────────────────────────────────────────┐ │
│ │ Chat Logs                                                                             │ │
│ │ ┌───────────────┐ ┌─────────────────────────────────────────────────────────────────┐ │ │
│ │ │ Filter chips  │ │ Hidden messages ▲  (Queued: 3 • Playing: 1)                     │ │ │
│ │ └───────────────┘ └─────────────────────────────────────────────────────────────────┘ │ │
│ │ ┌───────────────────────────────────────────────────────────────────────────────────┐ │ │
│ │ │ 12:45:03 │ alice [mod] │ Thanks for the raid!                                 [⋮] │ │ │
│ │ │-----------------------------------------------------------------------------------│ │ │
│ │ │ 12:45:10 │ bob │ !lurk (command filtered)                                     [⋮] │ │ │
│ │ │-----------------------------------------------------------------------------------│ │ │
│ │ │ 12:45:14 │ charlie │ Hola a todos                                             [⋮] │ │ │
│ │ │ ... (scrollable; active row stays centered with focus ring) ...                   │ │ │
│ │ └───────────────────────────────────────────────────────────────────────────────────┘ │ │
│ └───────────────────────────────────────────────────────────────────────────────────────┘ │
│ Playback strip: [▶ / ⏹ Confirm stop…]  [Inject message ▾]  Volume ▒▒▒▒░  [VU meter ▂▅▆▇▅] │
└───────────────────────────────────────────────────────────────────────────────────────────┘
```

Status strip instrumentation and placement will be finalized during UI prototyping; the mock illustrates one possible direction.

### Connection Onboarding

- Present a dedicated state when no chat sources are configured. Surface supported connections, requirements, and controls to add them.
- For each provider, show status (connected, action required, error) and next steps if user attention is needed.
- Allow multiple connections simultaneously (e.g., Twitch and YouTube). Indicate which sources currently feed the live session.

### Live Dashboard (Chat Logs View)

- **Navigation shell:** expose primary views—`Chat Logs`, `Chatters`, `Settings`, and `Advanced Settings`—through a top-aligned tab strip or an equivalent navigation pattern finalized during UI implementation. `Advanced Settings` remains hidden until explicitly enabled from `Settings`; shortcut behavior will be finalized during design prototyping to avoid accidental changes.
- **Chat log panel:** occupies the main viewport.
  - Each entry renders timestamp (source message time, tool-tip reveals full date-time), sender identifier (username, display name, color, highest-priority badge), and message content with emotes rendered using Twitch-compatible packs (BTTV, 7TV, FFZ).
  - Rows are selectable to support copy, replay, and moderation actions.
  - A right-aligned status control reflects the canonical lifecycle state defined in **Message Lifecycle Requirements** (e.g., `Arrived`, `Synthesizing`, `Queued`, `Playing`, `Played`, `Skipped`, `Error`) so the UI matches pipeline telemetry without maintaining a separate list. Context actions surface per-state affordances such as retry on `Error`, replay on `Played`, and skip while `Queued` or `Playing`. Tooltips expose diagnostic text and log references where applicable.
  - Additional menu actions (e.g., skip, change voice, open chatter profile) are available via context menu or secondary control.
  - When incoming volume exceeds the visible area, the message list scrolls independently from the surrounding layout. Show a pinned counter for the number of hidden messages and preserve primary viewport focus on the row currently playing.
  - Preserve keyboard focus on the row that matches the active playback item and auto-scroll so its status is visible while audio plays.
- **Playback strip:** fixed to the bottom edge.
  - `Play` toggles to `Stop` while active and starts immediate playback of newly eligible messages. When `Stop` is invoked while any message remains `Queued` or `Playing`, display a confirmation dialog with `Continue playback`, `Stop now`, and `Cancel`. Selecting `Stop now` halts the current item (if any) and leaves the remaining queue in place so operators can resume later; `Continue playback` dismisses the dialog without changing state.
  - Text input lets operators inject a message. By default the injected item plays back as the operator persona; the submit affordance exposes sender selection (existing chatter, synthetic persona, paid message simulation, or filters bypass test). Injection respects validation rules and is sequenced according to configured priority (default: plays next).
  - Global volume control adjusts output gain including optional boost above 100%.
  - Optional audio visualization mirrors current playback level to provide quick feedback.

### Chatters View

- Lists users seen during the session with metadata (roles, badges, message counts).
- Editable fields include voice override, display name override, and free-form notes (notes do not affect synthesis).
- Surface chatter-specific moderation history and link back to the originating messages for context.
- Provide inline search and filter controls so operators can quickly locate users by name, role, or override status.

### Settings View

- Accessible via three entry points: the standard system menu (e.g., macOS `Lupo ▸ Settings…`), the global shortcut documented below, and a discrete icon placed opposite the window chrome controls. Sections include:
  - **Connections:** add, edit, revoke, and reconnect chat or TTS providers. Display scopes that will be requested and the authentication method used.
  - **Message filtering:** whitelist or blacklist mode with filters for paid messages, user classes (subscriber tiers, members, followers), and pattern-based rules (prefix, keyword, or regex). Filters apply before synthesis. Default rules include (a) a general `!` prefix filter, (b) a dedicated moderator command prefix filter (defaults to `!lupo`; command catalog TBD and documented in help once available), and (c) suppression of provider-flagged bot accounts. Operators can disable the general rule while keeping command traffic or bot detection active through individual toggles.
  - **Command acceptance:** configure who can issue chat-based control commands. Options include `Nobody`, `Moderators` (default), or `Channel owner only`. The operator always retains full local controls regardless of this preference.
  - **Text transformation:** configurable template system to adjust spoken output (e.g., `{user} said {message}`) with localization-ready placeholders.
  - **TTS provider selection:** define provider priority, fallback rules, and per-provider configuration (voice sets, rate, pitch).
  - **Offline TTS strategy:** package Parler as the primary local engine with a default multilingual voice pack, expose download controls for optional languages, and document expected disk/CPU footprints. Summarize the trade-offs between offline (higher local CPU/GPU load, no provider spend) and online (billable usage, network-dependent latency) engines inline, then link to the built-in help topic that covers detailed cost/latency comparisons and instructions for running the latency test harness. Provide an opt-in ultra-light engine for constrained devices and surface availability of system TTS capture (only enabled on platforms where emitting WAV output is supported) so operators can choose between bundled and native voices.
  - **Voice strategy:** choose between per-user assignments, message-type overrides, or broadcast-wide defaults. Provide an editor for per-user overrides (display name, preferred voice, tuning parameters).
  - **Audio output:** select default or specific audio devices.
- Expose an entry point to `Advanced Settings` along with a warning about elevated risk. Access requires confirming a safety prompt and may optionally demand a modifier-key shortcut as a guard rail.

### Keyboard Shortcuts

- Provide platform-consistent defaults for core actions: quit (`Cmd+Q` on macOS, `Ctrl+Q` on Windows/Linux), toggle settings (`Cmd+,` / `Ctrl+,`), play or stop (`Space`), skip current message (`Cmd+Shift+S` / `Ctrl+Shift+S`), and adjust volume (`Cmd`/`Ctrl` + `Up` or `Down`). The keyboard-driven stop action surfaces the same confirmation flow described above whenever queued or active playback exists.
- Show the shortcut catalog in help mode and within relevant menu items; persist custom bindings when per-user overrides are introduced in later iterations.

### Help and Onboarding Aids

- Ship a platform help menu (e.g., macOS `Help`) with entries for an embedded manual and for toggling contextual help mode.
- Enable a contextual help mode that reveals hints for controls, explaining states and required user actions. Activate it from the Help menu or `Shift+Cmd+/` (`Shift+Ctrl+/` on Windows/Linux), then let users click any UI element to surface its description; exit with `Esc`.
- Provide tooltips for critical controls by default; deeper explanations appear when help mode is active.

### Advanced Settings View

- Presents an `about:config`-style interface to inspect and modify expert-level configuration keys (queue thresholds, concurrency caps, experimental flags) that are otherwise inaccessible.
- Requires explicit confirmation each time the view is opened and logs changes with timestamps so operators can roll back via history or restart.
- Group settings by domain (pipeline, telemetry, rendering, etc.) and annotate each key with type, default, and risk notes. Offer inline reset-to-default and export/import utilities to facilitate support escalations.
- Surface a non-destructive preview for high-risk changes (e.g., showing simulated queue behavior before applying) where feasible.

## Message Lifecycle Requirements

- Normalize each incoming provider event into a shared message schema to simplify downstream processing. Minimum fields:
  - `message_id`: locally generated UUID used by playback and persistence layers.
  - `provider`: enum value (`twitch_chat`, `youtube_live_chat`, `local`, …) plus `provider_message_id` for traceability (local injections omit the external identifier).
  - `channel_id`: unique identifier for the stream or chat room.
  - `user_id`: canonical provider user identifier. Display name, color, and badge data are retrieved separately by the chatter directory and referenced via this ID.
  - `source_timestamp`: provider-supplied message time; `received_at`: local UTC time.
  - `content`: normalized text payload with provider-specific tokens segmented so emote, emoji, and markdown handling can remain pluggable.
  - `delivery_kind`: enum capturing `standard`, `paid`, or other platform-flagged types.
  - `payment`: optional structure present when `delivery_kind` is `paid`, including `amount_minor_units`, `currency_code` (ISO 4217 or `bits`), `provider_tier_code`, and `provider_color_rank`. These initial fields will be validated against Twitch cheer tiers and YouTube super-chat color bands during the integration spike.
  - `metadata`: extensible map for platform-specific attributes (e.g., reply targets) that downstream features can opt into.

1. **Ingestion:** subscribe to each configured chat source, normalize incoming events, and timestamp them with both source time and receipt time. Push normalized messages into the `Arrived` queue.
2. **Filtering:** a single-threaded worker drains the `Arrived` queue, applies deterministic filters, and emits `Filtered` messages to the transformation queue. Provider lookups run in parallel to refresh role metadata so moderator-flagged senders are authenticated before their commands are honored and provider-flagged bot accounts can be auto-suppressed by default. Operator overrides triggered from the desktop UI are recorded before rule evaluation and force inclusion unless the operator explicitly marks the message as `Skipped`. Messages rejected by automation enter the `Skipped` terminal state with reason code and supporting telemetry. Moderator commands are parsed only when the message matches the configurable command prefix (default `!lupo`; command catalog TBD). Command payloads produce control events while the originating message remains filtered out unless the operator later overrides it. Moderator-issued actions are ignored whenever an operator override already exists for the target message, and upstream moderator deletions cause the associated message to transition to `Skipped(deleted_by_moderator)` unless it already reached `Played`, after which the operator may still replay it manually.
3. **Transformation:** a thread pool (default size equal to logical CPU count / 2, minimum 2) converts `Filtered` messages into speech-ready text, updating state to `Transformed` before dispatching to the synthesis queue.
4. **Synthesis:** a bounded worker pool (default concurrency 4) issues TTS jobs. Messages transition to `Synthesizing` while provider calls are in flight and to `Synthesized` when audio buffers are ready. Each worker pushes completion into the playback queue.
5. **Playback queue:** a dedicated playback thread maintains arrival ordering within each priority tier, advancing messages through `Queued`, `Playing`, and `Played` states. Operator overrides (force play, pin next, skip) take precedence over automated prioritization, followed by moderator commands, then default policy. When playback completes, messages move into a history log capped at 500 entries per session.
6. **Queue sizing and back-pressure:** every stage uses a bounded queue (crossbeam `ArrayQueue` or equivalent) with default maximums documented in the configuration schema. During transformation, lightweight heuristics (character count, language hints, selected voice rate) estimate the eventual audio duration so the scheduler can reject or defer work before synthesis begins. The running queue length combines synthesized durations with these predictions; if the threshold would be exceeded, the message pauses or transitions to `Skipped` (`predicted_queue_overflow`) unless the operator overrides via manual controls. A post-synthesis guard still checks actual durations to catch underestimation, using the `queue_overflow` reason code when triggered. Advanced users can modify queue depths and overflow strategy through an expert settings surface.
7. **Failure handling:** any stage can emit an `Error` state that records the last successful stage, provider or subsystem error code, and diagnostic message. Users can trigger manual retry, which re-enqueues the message at the appropriate stage based on recorded state. Skipped messages remain retryable via “unskip,” which returns them to the stage prior to filtering or, if already synthesized, to the playback queue.
8. **Telemetry:** record per-stage latency, retry attempts, and queue depth snapshots to support tuning of default thresholds. Emit structured logs for every transition into `Error`, `Skipped`, and manual override so incidents can be reconstructed.

## Integrations and Authentication

- General principles:
  - Use OAuth2 PKCE or token-based flows as recommended by each provider.
  - Attempt silent token refresh in the background using provider refresh tokens when permitted; renew five minutes before expiry by default. On refresh failures, surface a warning badge on the connection card that links directly to settings.
  - When background refresh is unavailable, expose a `Refresh` control in the provider settings entry that launches the full OAuth2 authorization flow.
  - Store long-lived credentials under the application data directory of each platform using a lightweight embedded database.
  - Present secrets to the rest of the application through an abstraction (e.g., `secrecy::SecretBox`) so upper layers remain agnostic of the underlying storage, and zero memory copies when secrets leave the abstraction.
  - Secrets are not exposed in plain text UI surfaces. Provide a revoke action that deletes locally cached tokens and clears the derived key material.
- Supported providers:
  - **Twitch Chat:** requires read access for the target channel and moderation scopes if history backfill is desired. Handle reconnects and history gaps after network loss.
  - **YouTube Live Chat:** supports concurrent operation with Twitch. Prompt the user to select the active broadcast if multiple exist.
  - **Bundled offline TTS (Parler):** ships with the installer as the primary local engine, preconfigured with a general-purpose multilingual voice set. Document disk footprint targets, GPU/CPU requirements, and optional voice pack downloads.
  - **Lightweight offline fallback:** include a minimal-asset engine for low-resource environments. The UI surfaces diminished quality expectations and allows operators to switch back to Parler when resources permit.
  - **Local message injection:** treat operator-authored or scripted messages as a first-class `local` provider. Local items honour the same filtering, prioritization, and persistence rules as external sources so the app remains fully usable offline.
  - **System TTS capture:** where the OS exposes APIs to render speech to a WAV/PCM stream (macOS `NSSpeechSynthesizer`, Windows SAPI `SpFileStream`, Linux Speech Dispatcher modules), expose it as a provider option with capability detection.
  - **Cloud TTS providers:** OpenAI, AWS Polly, Google Cloud Text-to-Speech, Azure Speech. Each provider can be enabled independently, and the UI must indicate quota usage or authentication expiry when reported by the provider.

## System Architecture Overview

- **Desktop shell:** renders UI with the Dioxus application (`app`, `lupo-app`) and coordinates user interactions.
- **Iced experiment:** `app2` (`lupo-app2`) remains available as a lighter, cross-platform shell experiment on a backburner track.
- **Connection manager:** owns authentication state, provider adapters, and reconnection policies.
- **Message pipeline:** implements ingestion, filtering, transformation, and queue management.
- **TTS engine abstraction:** routes synthesis requests to configured providers, returning standardized audio buffers and metadata.
- **Audio playback engine:** mixes and plays audio, supports real-time queue updates, and exposes global volume control.
- **Local persistence:** stores configuration, chatter overrides, and playback state on disk using workspace-approved mechanisms. Secrets are stored via platform-secure storage where feasible.

Workspace crates align with these components: `app` hosts the desktop shell, `audio` and `tts` (plus `tts-openai`) encapsulate playback and synthesis adapters, `models` and `migrator` cover persistence and migrations, and `secrets` wraps secure storage integrations.

## Reliability and Recovery

- Detect dropped connections and attempt exponential backoff reconnection while surfacing status to the user.
- Persist session state (active connections, message states, queue position, playback toggle) regularly so the app can restore after a restart.
- On relaunch, fetch missed chat messages when the provider allows history replay. If recovery fails, inform the operator and continue with new messages.
- Intercept quit requests when messages are still `Queued` or `Playing`, prompting the operator with `Quit anyway`, `Stop before quitting`, or `Don't quit`. `Quit anyway` persists the current state so it resumes on the next launch; `Stop before quitting` first runs the stop confirmation flow and quits only after playback halts; `Don't quit` simply dismisses the request.
- Protect against repeated crash loops by offering a safe-mode launch that disables playback and optional providers until the user re-enables them.

## Non-functional Considerations

- **Performance:** target end-to-end latency (message arrival to audio start) under configurable thresholds; default target is below five seconds for standard-length messages on stable connections.
- **Localization:** ensure UI strings and speech templates are translatable and support multilingual characters. Initial release may ship in a single language but should not block future localization.
- **Accessibility readiness:** choose UI components that support keyboard navigation, descriptive labels, and screen-reader hooks even if full accessibility testing is deferred.
- **Security:** avoid storing credentials in logs, and scope permissions to the minimum required to read chat and perform synthesis.

## Open Items

- Detailed user journeys and flow diagrams (e.g., first-run setup, connection loss recovery) remain to be documented.
- Queue sizing, retry policies, and provider-specific rate limits need numeric thresholds derived from technical feasibility.
- Final decisions on accessibility and localization rollout require cross-team alignment once initial release scope is set.

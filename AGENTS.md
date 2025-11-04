# Repository Guidelines

## Project Structure & Module Organization
Workspace crates keep responsibilities narrow:
- `app/`: Lupo desktop shell built with Dioxus; primary desktop app (`lupo-app`, bin `lupo`).
- `app2/`: Iced-based experimental shell (`lupo-app2`, bin `lupo2`).
- `cli/`: Command-line utilities for emote inspection and maintenance tasks.
- `models/`: SeaORM entities and shared domain types (export via `prelude.rs`).
- `migrator/`: migrations named `mYYYYMMDD_hhmmss_description.rs`; update `lib.rs` when adding one.
- `audio/`, `tts/`, `tts-openai/`: playback plumbing, engine traits, and the OpenAI adapter.
- `secrets/`: canonical source for secret keys and env overrides.
- `vendor/`: checked-in dependencies; `target/`: build artifacts (safe to clean).
- `xtask/`: custom task runner (`cargo xtask …`) that now owns packaging tasks.

## Product & Design References
- `DESIGN.md`: source of truth for product goals, UX structure, message lifecycle, and open questions. Align feature work and queue state handling with that document, and update it when behavior changes.
- UI work lands in the Dioxus shell under `app/`. Keep behavior aligned with `DESIGN.md` when iterating on interactions and queue state.

## Build, Test, and Development Commands
- `cargo build --workspace`: compile every crate with the shared `--cfg tokio_unstable` flag from `.cargo/config.toml`.
- `cargo run -p lupo-app`: start the desktop application (binary name `lupo`).
- `cargo fmt`: apply Rustfmt across the workspace; run before committing.
- `cargo clippy --workspace --all-targets -- -D warnings`: enforce lints; required for CI parity.
- `cargo test --workspace`: execute unit tests such as the `secrets` guards; use `-p crate-name` for focused runs.
- `cargo xtask bundle --app lupo [--open]`: build a macOS `.app` bundle for the default Dioxus shell. The command writes to `target/release/bundle/macos/...`.

## Coding Style & Naming Conventions
Use Rust 2024 defaults: four-space indentation, trailing commas, `snake_case` modules, and `UpperCamelCase` types. Re-export shared items through crate-level preludes. Prefer `tracing` with structured fields over ad-hoc logging. Migration files should echo the affected table or feature in both filename and module name.

## Testing Guidelines
Place `#[cfg(test)]` modules beside the code they verify and mirror public API signatures in names like `test_from_str_invalid`. Async tests should use `#[tokio::test(flavor = "multi_thread")]`. Persistence changes need migration assertions plus model round trips. Run `cargo test --workspace` before pushing; gate slow integrations with `#[ignore]` and a comment on how to enable them.

## Commit & Pull Request Guidelines
Commit messages follow short, sentence-cased summaries (“Build releases in Github Action.”); keep the subject under 72 characters and add optional detail in the body. Each pull request should link issues, outline functional impact, and include UI screenshots or recordings when behavior changes. Call out local testing commands so reviewers can reproduce results quickly.

## Secrets & Configuration Tips
Store the OpenAI API key with `lupo-cli secret set open_ai_api_key <value>` so the embedded secret store can surface it before launching `lupo`. Do not commit `.env` files. To add another secret, extend the `Secret` enum and its `iter_all_secrets` helper. Because we compile with `tokio_unstable`, note any new feature flags or cfgs when documenting async APIs.

## Dependency & Packaging Notes
- All third-party versions are declared once in the workspace `Cargo.toml`; use `package.workspace = true` or workspace dependency entries when adding crates.
- macOS bundles embed version strings from the workspace `Cargo.toml` and pick up `art/Icon-Lupo-TTS.icns`; remember to update the icon or plist logic there if assets move.
- After bundling, run `codesign -s - --deep --force <Lupo.app>` locally as needed; notarization and DMG creation can build on the existing xtask entry point.

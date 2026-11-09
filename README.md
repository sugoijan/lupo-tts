<img src="art/Icon-Lupo-TTS.svg" alt="logo" width="64" />

# LUPO TTS

LUPO stands for Linguistic User Pronounciation Operator. It is a little software to read live chat messages out loud with support for mixed language messages and special filters.

This project is licensed under Apache License 2.0. See [LICENSE](LICENSE).

In bocca al lupo.

## Features

- [ ] Connect to Twitch
- [ ] Use OpenAI "create speech" as TTS
- [ ] Queue messages from Twitch chat
- [ ] Settings fro TTS options
- [ ] Filtering system
- [ ] Override settings for specific users
- [ ] Detect Cheer-messages
- [ ] Ignore Twitch emotes
- [ ] Ignore 3rd-party (BTTV, FFZ, 7TV) emotes
- [ ] Support other TTS engines

## Known Issues

Active issues are tracked in [KNOWN_ISSUES.md](KNOWN_ISSUES.md).

## CLI Emote Utilities

The `lupo-cli twitch emotes` commands can inspect enabled third-party collections:

- `lupo-cli twitch emotes 7tv [--global|--channel <login>]`
- `lupo-cli twitch emotes bttv [--global|--channel <login>]`
- `lupo-cli twitch emotes ffz [--global|--channel <login>]`

Each variant fetches the corresponding provider's global set or the Twitch channel's emotes, echoing previews and modifier metadata when available.


## Developing

Rust 1.97 or newer is required.

The OBS plugin targets OBS Studio 33.0.1. After updating its vendored SDK, run
`cargo xtask obs-bindgen` to regenerate the committed bindings for the host target.
Regeneration needs libclang and SIMDe headers; set
`BINDGEN_EXTRA_CLANG_ARGS="-I/path/to/simde"` if the directory containing `simde/`
is outside Clang's default include paths. Normal builds use the committed bindings.

Current desktop shells:

- `cargo run -p lupo-app` runs the default Dioxus shell (`lupo`, webview renderer default).
- `cargo run -p lupo-app --no-default-features --features renderer-native` runs the native-feature variant (`lupo`).

Bundle commands:

- `cargo xtask bundle --app lupo`

Framework references:

- [Dioxus docs](https://docs.rs/dioxus/latest/dioxus/)

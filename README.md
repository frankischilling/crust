# crust

A native Twitch chat client written in Rust.

`crust` is a hobby project inspired by Chatterino, built as a multi-crate Rust workspace with an `egui` desktop UI, a Twitch IRC/WebSocket session layer, emote provider integrations, and local settings/log storage.

## Current status

Active early-stage project. The app builds and runs, and core chat workflows are in place. APIs and internals may still change.

## Features

- Twitch IRC over WebSocket (anonymous + authenticated modes)
- Multi-channel tabs (join, leave, reorder)
- Message rendering with:
	- Twitch emotes
	- BTTV / FFZ / 7TV emotes
	- Emoji tokenization via Twemoji URLs
	- URL and mention detection
- Emote catalog, picker, and autocomplete (`:` and Tab completion)
- Reply flow and basic moderation actions (timeout / ban UI paths)
- User profile popup with badges and account metadata
- Link preview metadata fetch (Open Graph / Twitter card style)
- Local settings persistence and optional keyring-backed token storage
- Per-channel append-only chat logs

## Workspace layout

- `crates/app` – binary entrypoint, runtime wiring, reducer/event loop
- `crates/ui` – `egui` application and widgets
- `crates/core` – shared domain models, events, tokenizer/highlight/state
- `crates/twitch` – IRC parser + Twitch session client/rate limiting
- `crates/emotes` – provider loaders and image cache (memory + disk)
- `crates/storage` – settings/token + log storage

## Requirements

- Rust stable toolchain (edition 2021)
- Cargo
- Linux desktop dependencies for `eframe`/`winit` (X11/Wayland)

## Build and run

From the workspace root:

```bash
cargo check
cargo run -p crust
```

Release build:

```bash
cargo run -p crust --release
```

## Authentication

- Anonymous mode works for read-only chat.
- To send messages, log in with a Twitch OAuth token in-app.
- Token storage uses OS keyring when available, with a settings-file fallback.

## Data paths

Using platform-specific app dirs via `directories::ProjectDirs` (typically):

- Config: `~/.config/crust/settings.toml`
- Cache: `~/.cache/crust/emotes/`
- Logs: `~/.local/share/crust/logs/`

## License

This project is licensed under GNU GPL v3.0. See [LICENSE](LICENSE).

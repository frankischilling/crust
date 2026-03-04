# crust

A native Twitch + Kick + IRC chat client written in Rust.

`crust` is a hobby project inspired by Chatterino, built as a multi-crate Rust workspace with an `egui` desktop UI, Twitch/Kick/IRC session layers, emote/badge integrations, and local settings/log storage.

## Screenshots

![Chat view](img/demo.png)

![Emote picker](img/demo2.png)

![User profile popup](img/demo4.png)

![Multi-channel tabs](img/demo5.png)

![Windows built binary](img/demo6.png)

## Current status

Active early-stage project. The app builds and runs, and core chat workflows are in place. APIs and internals may still change.

## Features

- Twitch IRC over WebSocket - anonymous and authenticated modes
- Kick chat over Pusher WebSocket (read-only)
- Generic IRC support (plain + TLS) via `irc://host[:port]/channel` and `ircs://host[:port]/channel`
- Multi-channel tabs - join, leave, reorder channels (Twitch + Kick + IRC)
- Multi-account support - add, switch, remove, and set a default account
- Message rendering:
  - Twitch native emotes
  - Third-party emotes: BTTV, FFZ, 7TV (global + channel + personal sets)
  - Kick emotes (Kick-native + inline tag fallback)
  - Animated emote support (GIF, WebP)
  - Emoji tokenization via Twemoji URLs
  - Badge rendering:
    - Twitch global + channel badges via IVR
    - Kick badge image rendering with channel-level API fallback
  - URL and @mention detection
  - Highlighted and first-message indicators
- Emote picker and `:` autocomplete with Tab completion
- Reply flow (threaded replies)
- Basic moderation: timeout, ban, unban
- User profile popup with avatar, badges, account metadata, and recent messages (Twitch + Kick)
- Link preview metadata fetch (Open Graph / Twitter card)
- Message input history (arrow-key recall)
- Local settings persistence and optional keyring-backed token storage
- Per-channel append-only chat logs
- Chat history on join (via recent-messages.robotty.de / IVR fallback)

## Workspace layout

- `crates/app` - binary entrypoint, runtime wiring, reducer/event loop
- `crates/ui` - `egui` application and widgets
- `crates/core` - shared domain models, events, tokenizer/highlight/state
- `crates/twitch` - IRC parser + Twitch session client/reconnect/rate limiting
- `crates/kick` - Kick session client (Pusher), channel metadata and chat event parsing
- `crates/emotes` - provider loaders and image cache (memory + disk)
- `crates/storage` - settings/token + log storage

## Requirements

- Rust stable toolchain (edition 2021)
- Cargo
- Linux desktop dependencies for `eframe`/`winit` (X11 or Wayland), or
- Windows C++ build tools (MSVC toolchain)

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

### Performance testing

Lightweight performance tests are available as ignored test cases (so they
don't run in normal `cargo test`):

```bash
cargo test -p crust-core --release perf_ -- --ignored --nocapture
cargo test -p crust-twitch --release perf_ -- --ignored --nocapture
```

These print simple throughput metrics (ops/sec) for tokenization/highlighting
and Twitch IRC parsing hot paths.

Replay soak test (ignored by default):

```bash
CRUST_SOAK_RATE=200 CRUST_SOAK_SECS=900 \
  cargo test -p crust-twitch --release replay_soak_ -- --ignored --nocapture
```

PowerShell:

```powershell
$env:CRUST_SOAK_RATE=200
$env:CRUST_SOAK_SECS=900
cargo test -p crust-twitch --release replay_soak_ -- --ignored --nocapture
```

Soak-test performance indicators (printed at the end of the run):

- `ratio` should stay close to `1.000` (consumer keeping up with producer)
- `parse_errors` should be `0`
- `final_backlog` should return to `0` (or near-zero)
- `max_backlog` should stay small and stable (no unbounded growth)
- `max_frame_work_ms` should remain low; lower values indicate better per-frame headroom

Example healthy output:

```text
[soak] done: produced=179999, consumed=179999, ratio=1.000, parse_errors=0, max_backlog=9, final_backlog=0, peak_frame_processed=9, max_frame_work_ms=1.529
```

### Windows (native)

You can build and run `crust` directly on Windows with Cargo.

Install prerequisites with Chocolatey (PowerShell **as Administrator**):

```powershell
choco install -y rustup.install visualstudio2022buildtools visualstudio2022-workload-vctools git
rustup default stable-x86_64-pc-windows-msvc
```

Then build/run from the repo root:

```powershell
cargo check
cargo run -p crust
```

Release:

```powershell
cargo run -p crust --release
```

If you hit linker errors like `LNK1318` (PDB/file-system limits), free disk space and retry (or run `cargo clean` first).

### Running on WSL

Requires VcXsrv launched with the `-wgl` flag (or "Native opengl" checked in XLaunch) to expose GLX framebuffer configs. Mesa version overrides are needed to negotiate a valid OpenGL context:

```bash
export DISPLAY=172.17.128.1:0.0  # replace with your host IP - check /etc/resolv.conf nameserver
export MESA_GL_VERSION_OVERRIDE=3.3
export MESA_GLSL_VERSION_OVERRIDE=330
export WINIT_UNIX_BACKEND=x11
unset WAYLAND_DISPLAY
cargo run -p crust --release
```

**WSLg (Windows 11)** - works out of the box with Wayland, no X server or overrides needed:

```bash
cargo run -p crust --release
```

## Authentication

- Anonymous mode works for read-only chat.
- To send messages, log in with a Twitch OAuth token in-app.
- Multiple accounts are supported - switch accounts without restarting.
- Token storage uses the OS keyring when available, with a settings-file fallback.
- Kick currently runs in read-only mode (sending messages to Kick is not yet implemented).

## Joining channels

- Twitch: `channelname` or `twitch:channelname`
- Kick: `kick:channelname`
- IRC: `irc://host[:port]/channel` or `ircs://host[:port]/channel`

## Notes

- Kick profile lookups use Kick public APIs. If a profile endpoint is temporarily unavailable/forbidden, the app falls back to a minimal profile card instead of hanging on loading.

## Data paths

Using platform-specific app dirs via `directories::ProjectDirs` (typically):

- Config: `~/.config/crust/settings.toml`
- Cache: `~/.cache/crust/emotes/`
- Logs: `~/.local/share/crust/logs/`

## License

This project is licensed under GNU GPL v3.0. See [LICENSE](LICENSE).

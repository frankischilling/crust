# crust

A native Twitch chat client written in Rust.

`crust` is a hobby project built as a multi-crate Rust workspace with an `egui` desktop UI, a Twitch IRC/WebSocket session layer, emote provider integrations, Lua plugin support, and local settings/log storage.

## Screenshots

![Chat view](img/demo.png)

![Emote picker](img/demo2.png)

![User profile popup](img/demo4.png)

![Multi-channel tabs](img/demo5.png)

![Mod tabs](img/demo8.png)

## Current status

Active early-stage hobby project with daily-use chat workflows in place. The app builds and runs, plugin APIs are available, and built-in auto-updater support is now implemented for Windows and Debian Linux systems. APIs and internals may still evolve.

Kick support is currently super cooked and munted (very incomplete / unstable).

## Documentation

- [Crust docs home](docs/HOME.md)
- [Features and keybinds](docs/FEATURES_AND_KEYBINDS.md)
- [Plugin API reference](docs/API.md)
- [Release notes v0.5.1](docs/releases/Release_v0.5.1.md)
- [Release notes v0.5.0](docs/releases/Release_v0.5.0.md)
- [Release notes v0.4.9](docs/releases/Release_v0.4.9.md)
- [Release notes v0.4.5](docs/releases/Release_v0.4.5.md)

## Features

### Platforms and transports

- Twitch chat via IRC over WebSocket (anonymous and authenticated modes)
- Twitch EventSub coverage: subs / gift subs / raids / channel-points redemptions / poll + prediction lifecycle / stream `online` + `offline` / hype-train begin/progress/end / moderator + AutoMod + suspicious-user + whisper topics, with `session_reconnect` continuity, 10 000-entry event-id LRU dedupe, and per-reconnect backfill
- Kick chat integration (beta / currently incomplete)
- Generic IRC integration (beta), including IRC server tabs and keyed channel joins
- IRC channel redirect handling (old channel target -> new channel target)

### Channels, tabs, and navigation

- Multi-channel management: join, leave, reorder, and close channels
- Sidebar and top-tab channel layouts
- Tab style controls (compact/normal), optional close buttons, and optional live indicators
- Unread and mention counters for channels
- Quick switch palette (Ctrl+K):
  - mention-first ordering (mentions -> unread -> others)
  - unified channel + whisper-thread list
  - query aliases for platforms and whispers
  - keyboard navigation and activation
- Split-pane chat view (up to 4 panes):
  - drag channel to split
  - focused-pane tracking
  - split header controls and per-pane search toggle
  - split keyboard navigation and pane reordering shortcuts

### Desktop UI shell

- Join dialog and account/login switcher dialogs
- User profile popup and dedicated moderation tools window
- Whisper management window
- Optional analytics side panel
- Optional IRC status diagnostics panel
- Optional performance overlay for chat rendering stats
- Startup loading screen for initial prefetch visibility

### Messages and rendering

- Message types rendered in chat include:
  - standard chat and actions
  - subscriptions, raids, bits, channel points redemptions
  - hype train begin/progress/end with progress bar + level + top contributor (`channel.hype_train.*` EventSub)
  - shared chat (cross-channel mirror) with source-channel chip, source mod/vip badges, and notification suppression for already-open source channels
  - timeout/ban/clear notices and system notices
  - first-message and pinned-message indicators
- Inline reply/thread support with reply metadata
- Rich span/token rendering:
  - text
  - emotes
  - badges
  - mentions
  - URLs
  - emoji
- URL detection and Open Graph/Twitter card preview metadata
- Message deletion and user-message clear handling in UI
- Optional timestamp rendering (12h/24h, with/without seconds)
- Optional long-message collapse with configurable line limits

### Input and command UX

- Message input history recall (arrow keys)
- `:` emote autocomplete and Tab completion
- Slash command handling for chat, moderation, and creator workflows
- Command usage tracking for ranking/autocomplete improvements
- Custom command aliases (`/hi = /me says hi {1}`) with `{channel}`, `{user}`, `{input}`, `{streamer}` variables
- Spellcheck on the input box (Norvig + Soundex + QWERTY proximity) with right-click suggestions and a personal dictionary
- Image paste / drag-drop uploader (Imgur, Nuuls, ShareX SXCU, or any custom multipart endpoint)

### Emotes, badges, and cosmetics

- Emote providers:
  - Twitch native
  - BTTV
  - FFZ
  - 7TV
  - Kick
- Provider-aware emote resolution and rendering priority
- Animated emote support (GIF/WebP)
- Emoji tokenization/rendering via Twemoji URLs
- Badge rendering from global/channel badge sources
- 7TV sender cosmetics support (name paints/badge updates)
- Emote picker features:
  - favorites
  - recents
  - provider preference boost
- Emote image caching in memory and on disk

### Whispers

- Twitch whisper receive/render path
- Whisper thread list with recency ordering
- Per-thread unread counts and mention counts
- Whisper compose/open workflow from thread context
- Whisper desktop notifications (when enabled)

### Moderation and creator tools

- Message-row right-click moderation quick actions:
  - Quick Delete
  - Quick Timeout 10m
  - Quick Ban
  - Quick Warn
- Full moderation actions:
  - timeout/ban/unban/warn
  - delete message
  - clear user messages locally
  - suspicious-user / low-trust monitor + restrict + clear
  - AutoMod message approve/deny
  - unban request fetch/resolve
- Moderation presets for reusable actions
- Moderation tools window with dedicated AutoMod / Low-trust / Unban-requests tabs:
  - per-tab item-count badges
  - shared filter strip (login or message text)
  - per-row approve / deny actions and bulk approve / bulk deny
  - keyboard nav: `J` / `K` focus, `A` / `D` approve / deny, `Shift+A` / `Shift+D` bulk, `Tab` / `Shift+Tab` switch tab
- Creator tools:
  - polls (create/end/cancel)
  - predictions (create/lock/resolve/cancel)
  - commercials
  - stream markers
  - announcements
  - shoutouts
  - reward redemption status updates
  - auto-claim of channel-points "Bonus Points" button (Crust-only; opt-in, via embedded Twitch webview)

### Profiles, stream state, and notifications

- User profile popup with account metadata, avatar/badges, recent messages, follow age, account age, shared-channels list, and prior-timeouts log in moderator view
- Stream status fetch and live/offline updates
- Stream watch/tracker behavior for channel presence indicators
- Live / followed-channels feed virtual tab fed by `stream.online` / `stream.offline` EventSub
- Mentions-only cross-channel feed tab with restart-survival via local SQLite log
- Tab visibility rules ("Hide when offline" per-channel)
- Desktop notifications for mentions/highlights/whispers (configurable)
- In-app event toast banners for high-visibility events
- Per-event sound notifications (mention / whisper / subscribe / raid / custom highlight) with file-path + volume sliders, baked-in synthetic ping fallback, and streamer-mode mute integration

### Accounts, auth, and settings

- Multi-account support:
  - add/switch/remove accounts
  - set default startup account
- Auth refresh flow for expired/invalid sessions
- OS keyring-backed token storage with settings-file fallback
- IRC identity settings (nick and NickServ credentials)
- Settings persistence for:
  - general behavior (timestamps, local log indexing, auto-join, ignores/highlights)
  - appearance/layout
  - per-section font sizes (chat body, UI scale, topbar, tabs, timestamps, pills, popups, chips, usercard, dialog)
  - chat UI behavior
  - notification preferences
  - emote picker preferences (favorites, recents, provider boost)
  - highlight rules
  - filter records (typed expression DSL with `author.*`, `message.*`, `channel.*`, `flags.*`, `has.*`)
  - ignored users + ignored phrases
  - mod action presets
  - sound events (per-event enabled / file path / volume)
  - hotkey bindings (rebindable, conflict detection)
  - command aliases
  - tab visibility rules
  - image uploader endpoint config
  - external tools (Streamlink path, quality, extra args, custom player template)
  - streamer mode (link previews, viewer counts, sound suppression while live)
  - beta feature flags

### Search, history, and local data

- Recent-message history load on join (recent-messages API with IVR fallback)
- Per-channel local append-only log indexing (SQLite)
- Older local history paging support
- Per-channel message search/filter UI
- Data directories via `directories::ProjectDirs` for config/cache/logs

### Plugin platform (Lua)

- Lua plugin runtime with plugin lifecycle management
- Plugin command registration and execution
- Event callback registration across account/chat/settings/moderation/UI events including `SoundSettingsUpdated`, `HotkeyBindingsUpdated`, `UploadStarted`, `UploadFinished`, `FontSettingsUpdated`
- Plugin UI surfaces:
  - custom windows
  - settings pages
  - host-panel extensions
- Host callback helpers for image fetch, profile fetch, link preview fetch, and IVR log fetch
- Filter engine bridge: `c2.filters_parse(expr)` / `c2.filters_evaluate(expr, ctx)` for the typed expression DSL
- Image uploader bridge: `c2.upload_image(channel, bytes_b64, format, source_path?)`
- Sound + hotkey bridges: `c2.set_sound_settings(events)` / `c2.get_sound_settings()` and `c2.set_hotkey_bindings(bindings)` / `c2.get_hotkey_bindings()`
- Example plugin set in [plugins](plugins/) including `c9_api_expansion_demo` exercising every new surface

### Updater and release flow

- Windows and Debian Linux auto-updater via GitHub Releases:
  - stable-only checks
  - SHA256 verification
  - staged install prompt flow
  - version skip support
- Background and manual update check flows
- PowerShell launch fallback chain for installer reliability

### Reliability

- Crash handler with persistent reports under `{data_dir}/logs/crashes/`:
  - structured panic reports (version, target triple, run id, OS / arch / display server, pid, thread, payload, force-captured backtrace, last ~512 tracing events, live settings snapshot)
  - synthetic "abnormal exit" reports on SIGKILL / power loss / native crash detected via session sentinel
  - rolling 20-report + 40-sentinel retention
  - in-app crash viewer with View / Copy / Show on disk / Delete / Dismiss / Restart actions

## Workspace layout

- `crates/app` - binary entrypoint, runtime wiring, reducer/event loop
- `crates/ui` - `egui` application and widgets
- `crates/core` - shared domain models, events, tokenizer / highlight / filter DSL / hotkeys / sound / state
- `crates/twitch` - IRC parser + Twitch session client / reconnect / rate limiting / EventSub / Helix / GraphQL providers
- `crates/kick` - Kick chat session (beta)
- `crates/emotes` - provider loaders and image cache (memory + disk)
- `crates/storage` - settings / token / log storage and persistence
- `crates/uploader` - image-upload backend (Imgur, Nuuls, ShareX SXCU, custom)
- `crates/webview` / `crates/webview-host` - embedded Twitch webview for auth + auto-claim flows

## Requirements

- Rust stable toolchain (edition 2021)
- Cargo
- Linux desktop dependencies for `eframe`/`winit` (X11 or Wayland)
- Linux dev dependencies for the embedded Twitch webview sidecar (`crust-webview`): WebKitGTK 4.1, libsoup 3, GTK 3
  - Ubuntu / Debian: `sudo apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev libgtk-3-dev libjavascriptcoregtk-4.1-dev pkg-config build-essential`
  - Fedora: `sudo dnf install webkit2gtk4.1-devel libsoup3-devel gtk3-devel pkgconf-pkg-config`
  - Arch: `sudo pacman -S webkit2gtk-4.1 libsoup3 gtk3 pkgconf`
- Windows 10/11 or Debian-based Linux for built-in auto-install updates
- Windows: WebView2 Runtime (preinstalled on Windows 11; Windows 10 may need the Evergreen runtime from Microsoft)

## Build and run

From the workspace root:

```bash
cargo check
cargo run -p crust
```

Release build:

```bash
cargo build -p crust --release
```

### Windows release binary

Build and package a Windows release zip from PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\build_windows_release.ps1
```

Artifacts are produced at:

- `target\\release\\crust.exe`
- `dist\\windows\\crust-v<version>-windows-x64.zip`

### Debian release package

Build and package a Debian release `.deb` from Linux:

```bash
bash ./scripts/build_debian_release.sh
```

Artifacts are produced at:

- `target/release/crust`
- `dist/debian/crust-v<version>-debian-<arch>.deb`
- `dist/debian/crust-v<version>-debian-<arch>.deb.sha256`

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

## Data paths

Using platform-specific app dirs via `directories::ProjectDirs` (typically):

- Config: `~/.config/crust/settings.toml`
- Cache: `~/.cache/crust/emotes/`
- Logs: `~/.local/share/crust/logs/`

## License

This project is licensed under GNU GPL v3.0. See [LICENSE](LICENSE).

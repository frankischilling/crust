# crust

A native Twitch chat client written in Rust, with an `egui` desktop UI.

Crust is an early-stage hobby project. It supports multiple channels and accounts,
split chat panes, detached windows, moderation tools, emotes, and Lua plugins.
APIs and internals may change. Kick support is incomplete and unstable; generic IRC support is also
in beta.

## Features

- Twitch chat over IRC/WebSocket, with anonymous reading and authenticated chat
- Tabs, a sidebar, nested horizontal and vertical chat panes, and a quick switch palette
- Detached chat windows and transparent overlays with independent drafts, search, and filters
- Twitch, BTTV, FFZ, and 7TV emotes, including animation and overlays, badges, and sender cosmetics
- Reply threads, whispers, search, local history, highlights, and notification sounds
- Moderation queues, user cards, polls, predictions, and channel-points tools
- Configurable shortcuts, command aliases, spellcheck, image uploads, and Streamlink integration
- Lua commands, event callbacks, plugin windows, and settings pages
- Update checks and installation support for Windows and Debian-based Linux

The [feature and keybind reference](docs/FEATURES_AND_KEYBINDS.md) describes these
features, their settings, and the available shortcuts and slash commands.

## Build and run

Install a stable Rust toolchain and Cargo. Linux builds also need the desktop and
WebKitGTK development packages listed in [Building Crust](docs/BUILDING.md).
Windows uses the WebView2 Runtime, which is included with Windows 11 and may need
to be installed on Windows 10.

From the workspace root:

```bash
cargo check
cargo run -p crust
```

See [Building Crust](docs/BUILDING.md) for release builds, Windows and Debian
packaging, WSL setup, and the workspace layout.

## Authentication

- Anonymous mode works for read-only chat.
- To send messages, log in with a Twitch OAuth token in-app.
- Multiple accounts are supported; switch accounts without restarting.
- Tokens are saved in the settings file and also in the OS keyring when available.

## Screenshot

![Chat view](img/demo.png)

See the [interface audit](docs/AUDIT_UI_PERFORMANCE.md#native-interface-captures)
for current settings and account-dialog captures.

## Documentation

- [Documentation index](docs/HOME.md)
- [Features and keybinds](docs/FEATURES_AND_KEYBINDS.md)
- [Chat recovery, previews, and image loading audit](docs/AUDIT_CHAT_RECOVERY_AND_PREVIEWS.md)
- [Plugin API reference](docs/API.md)
- [Example plugins](plugins/)
- [Release notes v0.7.2](docs/releases/Release_v0.7.2.md)

## Data paths

Crust uses platform-specific app directories from `directories::ProjectDirs`.
Typical Linux paths are:

- Config: `~/.config/crust/settings.toml`
- Cache: `~/.cache/crust/emotes/`
- Logs: `~/.local/share/crust/logs/`

## License

This project is licensed under GNU GPL v3.0. See [LICENSE](LICENSE).

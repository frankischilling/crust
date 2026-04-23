# Crust v0.5.0 Release Notes

Date: 2026-04-22

v0.5.0 is the biggest Chatterino-parity drop to date. It lands nine new subsystems (image uploader, Streamlink, live-channels feed, mentions tab, tab visibility rules, command aliases, hotkey editor, spellcheck with user dictionary, sound event system) and a production-grade crash reporter, plus a round of UI and plugin-runtime fixes.

## Highlights

### New features

- **Image uploader.** Paste an image from the clipboard or drop a file on the chat input and Crust POSTs it to a configured host (Imgur, Nuuls, a ShareX SXCU template, or any custom multipart endpoint) and inserts the returned URL at the caret. Response parsing uses dotted-path JSON extraction so arbitrary hosts can be described by config alone. Deletion URLs are appended to a local history file. A lightweight image preview shows the staged upload before it is sent, and upload failures surface as a retryable toast. Config lives under `[uploader]` in settings.
- **Streamlink integration.** Right-click any Twitch tab to Open in Streamlink, or bind a custom player template (mpv, VLC, etc.) with a `{channel}` variable. `[external_tools]` settings cover the streamlink path, default quality, and extra args. The `/streamlink` slash command uses the same path.
- **Live / followed-channels feed.** A new "Live" virtual tab aggregates every followed channel currently live, fed by `stream.online` / `stream.offline` EventSub topics. Rows show thumbnail, title, category, and viewer count; clicking jumps to an existing tab or opens a new one. Updates land within ~15 s of a state change.
- **Mentions-only tab.** A persistent cross-channel mention feed. Every highlight or mention-matching message from any open channel is copied into a 2 000-row ring buffer, rendered with a clickable channel pill, and survives restarts by replaying from the local SQLite log on startup.
- **Tab visibility rules.** Right-click a Twitch tab to set "Hide when offline". Hidden tabs disappear from the top strip and sidebar while the stream is offline and reappear automatically when it goes live. Per-channel rules are persisted under `[tab_visibility_rules]`.
- **Custom command aliases.** Define `/hi = /me says hi {1}` style aliases in a new Settings > Commands page. Variables include `{1}`, `{1+}`, `{channel}`, `{user}`, `{input}`, and `{streamer}`. Built-in commands still win the lookup; recursive alias chains are detected and rejected before send.
- **Hotkey editor with remappable shortcuts.** Every keyboard shortcut now routes through a central hotkey registry keyed by `Action`. Rebind any binding from Settings > Hotkeys with a capture-key row; conflicts surface an inline warning before they clash. Bindings round-trip through settings.
- **Spellcheck with user dictionary.** The chat input now paints red wavy underlines on misspelled words. Right-click a word for the top 5 suggestions or "Add to dictionary", and words added to your personal dict are persisted in settings and never flag again. A Settings > Chat > Spell check section hosts the enable toggle plus a custom-word editor.
- **Sound event system.** Mention, whisper, subscribe / resub / gift sub, raid, and highlight-rule events can now play a per-event sound at a per-event volume. Includes a baked-in synthetic ping so it works with zero configuration, file-path + volume sliders per event, and an in-UI preview button. Integrates with streamer mode so sounds can be auto-silenced while broadcasting.
- **Crash handler + crash reporter.** Production-grade Rust crash capture with a "view on next launch" flow.
  - `std::panic::set_hook` writes a structured report into `{data_dir}/logs/crashes/` with app version, build profile, target triple, run id, UTC + local timestamps, OS / arch / family, CPU count, display server (Wayland / X11 / etc.), pid, thread name, panic payload + source location, a force-captured `Backtrace`, the last ~512 tracing events, and a live settings snapshot.
  - Tracing-subscriber log ring captures the tail of log output so the report has the runtime context leading up to the crash.
  - Settings snapshot stays fresh because `crust-storage` now fires a persist hook on every `save()`.
  - Abnormal-shutdown detection: a `{run_id}.session` sentinel is written on startup and deleted on clean exit, so SIGKILL / power loss / native crashes produce a synthetic "abnormal exit" report on next launch.
  - Double-panic guard and `catch_unwind` around the hook body keeps a panic inside the panic hook from aborting the process before the first report flushes.
  - Disk-bloat ceiling: rolling 20-report + 40-sentinel retention.
  - New in-app crash viewer auto-opens when orphan reports exist. Actions: View, Copy to clipboard, Show file on disk, Delete, Dismiss all, Open folder, and Restart Crust (clears the session sentinel before re-spawning so the restart itself never reports as abnormal).

### Bug fixes

- **Reward-emote rendering.** Channel-points reward posts that ship only a link to a 7TV emote page (`https://7tv.app/emotes/<id>`) now render the actual animated emote inline instead of a bare URL, so the redeemed emote is visible in the reward row.
- **Plugin UI race.** Plugin panels, windows, and settings pages could previously double-register or flicker when a plugin reloaded while the UI was mid-frame. Host and plugin callbacks are now routed through a single-writer lock so reloads can't publish partial surface sets into the UI thread.
- **Crash-handler restart.** The in-app "Restart Crust" button now clears the session sentinel before re-spawning, so a user-initiated restart never appears in the next launch as an abnormal shutdown.

### UI fixes

- **Crash viewer.** Panic reports render in red with a `PANIC` badge; synthetic abnormal-exit reports render in amber with an `ABNORMAL EXIT` badge so the two are visually distinct. The "Open crash folder" action resolves to the correct parent directory for both report files and session sentinels.
- **Live feed panel.** Sort order, thumbnail sizing, and row padding tuned to match the rest of the channel-list styling; row state updates in place instead of re-drawing on every EventSub tick.
- **Mentions tab.** Channel pill now uses the same color as the source channel's tab, and clicking a mention scrolls the destination message into view instead of just activating the channel.
- **Settings pages.** New Commands / Hotkeys / Spell check / Notifications / Uploader / External tools sections share the existing Settings styling. Long spellcheck suggestion lists truncate at 5 entries to keep the right-click menu compact.
- **Chat input.** Drag-drop and paste-image feedback shows a small thumbnail preview above the caret while the upload is in flight. Autocomplete placement no longer jumps when the preview appears and disappears.

## Slash command reference

Full catalogue in `docs/FEATURES_AND_KEYBINDS.md`. New in v0.5.0:

- `/streamlink [channel]`: open the given (or current) Twitch channel in Streamlink using the configured path / quality / extra args.
- Any user-defined alias (`/hi`, `/rev`, etc.) registered in Settings > Commands is resolved after the built-in table.

## Settings additions

- `[uploader]`: `endpoint`, `request_form_field`, `image_link`, `deletion_link`, `extra_headers`.
- `[external_tools]`: `streamlink_path`, `quality`, `extra_args`, `player_template`.
- `[tab_visibility_rules]`: per-channel visibility rule map.
- `[command_aliases]`: trigger to body pairs.
- `[hotkeys]`: action to key-binding map.
- `[sounds]`: per-event `enabled` / `path` / `volume`.
- `spellcheck_enabled: bool` and `custom_spell_dict: Vec<String>`.

All new fields ship defaults, so existing configs pick them up on first load with no manual migration.

## Upgrade notes

- Crash reports are written under `{data_dir}/logs/crashes/` (Windows: `%APPDATA%\crust\logs\crashes\`, Linux: `~/.local/share/crust/logs/crashes/`, macOS: `~/Library/Application Support/crust/logs/crashes/`). Delete them any time; the viewer can also do it for you.
- The image uploader ships no credentials. Set up an endpoint (Imgur, Nuuls, or your own) under Settings > Image uploader before paste-to-upload will work.
- Streamlink itself is still an external dependency. Install via `pip install streamlink` (or your distro package) and point Crust at the binary.
- Hotkey rebinds round-trip through settings, so if a machine-specific binding breaks during an update, open Settings > Hotkeys > Reset to defaults.
- Command aliases are case-insensitive on the trigger side. Aliases that resolve to other aliases are rejected at save time with an inline error.

## Notes

- Workspace and internal crate versions bumped to 0.5.0.
- The new `crust-uploader` crate is a first-class workspace member alongside `crust-core`, `crust-ui`, `crust-twitch`, `crust-emotes`, `crust-kick`, and `crust-storage`.
- EventSub and IRC transport behavior from v0.4.9 carry forward unchanged.
- Local SQLite log format is unchanged, and the Mentions tab backfill is gated on the existing `local_log_indexing_enabled` setting.

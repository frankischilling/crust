# Features and keybinds

This reference covers chat features, default shortcuts, mouse actions, and slash
commands. Kick and generic IRC support remain beta features.

## Feature overview

### Platforms and transports

- Twitch chat via IRC over WebSocket (anonymous and authenticated modes)
- Twitch EventSub events: subs / gift subs / raids / channel-points redemptions / poll + prediction lifecycle / stream `online` + `offline` / hype-train begin/progress/end / moderator + AutoMod + suspicious-user + whisper topics, with `session_reconnect` continuity, deduplication across 10,000 recent event IDs, and backfill after reconnect
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
- Spellcheck on the input box (Norvig + Soundex + QWERTY proximity), with up to five right-click suggestions and a persistent personal dictionary
- Image paste / drag-drop uploader (Imgur, Nuuls, ShareX SXCU, or any custom multipart endpoint), with deletion URLs logged locally

### Emotes, badges, and cosmetics

- Emote providers:
  - Twitch native
  - BTTV
  - FFZ
  - 7TV
  - Kick
- Emote resolution by provider and configured priority
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

- Twitch whisper sending and receiving
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
  - optional automatic claiming of channel-points "Bonus Points" through the embedded Twitch webview

### Profiles, stream state, and notifications

- User profile popup with account metadata, avatar/badges, recent messages, follow age, account age, shared-channels list, and prior-timeouts log in moderator view
- Optional pronoun lookup when opening a user card
- Stream status fetch and live/offline updates
- Stream tracking for channel presence indicators
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
  - user nicknames
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

Expression filters can match shared chat and whispers with `flags.shared` and
`flags.whisper`. Hide rules remove matching messages from chat; dim rules leave
them visible with reduced emphasis.

### Search, history, and local data

- Recent-message history load on join (recent-messages API with IVR fallback)
- Per-channel local append-only log indexing (SQLite)
- Older local history paging support
- Per-channel message search/filter UI
- Cross-channel search with predicates such as `from:<user>`, `has:link`, `in:<channel>`, and `regex:"<pattern>"`
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
- Example plugins in [plugins](../plugins/), including `c9_api_expansion_demo` for the filter, uploader, sound, and shortcut APIs

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

## Keybinds

The tables below show default shortcuts. Settings > Hotkeys lets you rebind
registered app actions and reports conflicts. Text editing, autocomplete, and
palette navigation also use the widget shortcuts listed below.

### Global and navigation

| Keybind | Behavior |
| --- | --- |
| Ctrl+K | Open quick switch palette |
| Ctrl+F | Open message search on active channel/pane |
| Ctrl+Shift+F | Open cross-channel search popup (predicate DSL) |
| Escape | Close quick switch or close search (when open) |
| Ctrl+Tab, Ctrl+PageDown, Alt+Right | Next channel (prioritizes mentions/unread) |
| Ctrl+Shift+Tab, Ctrl+PageUp, Alt+Left | Previous channel (prioritizes mentions/unread) |
| Ctrl+1 .. Ctrl+9 | Jump directly to channel tab index 1..9 |
| Ctrl+Home | Focus first channel (or first split in split mode) |
| Ctrl+End | Focus last channel (or last split in split mode) |
| Alt+Shift+Left | Move active channel tab left (or move focused split left in split mode) |
| Alt+Shift+Right | Move active channel tab right (or move focused split right in split mode) |
| Ctrl+= | Increase chat font size on focused split |
| Ctrl+- | Decrease chat font size on focused split |
| Ctrl+0 | Reset chat font size on focused split |

### Split-mode only

| Keybind | Behavior |
| --- | --- |
| Ctrl+Alt+PageUp | Focus previous split |
| Ctrl+Alt+PageDown | Focus next split |
| Ctrl+Alt+Shift+Left | Move focused split left |
| Ctrl+Alt+Shift+Right | Move focused split right |

### Quick switch palette

| Keybind | Behavior |
| --- | --- |
| Ctrl+K | Open palette |
| Up / Down | Move selection |
| Enter | Activate selected channel/thread |
| Escape | Close palette |

### Chat input and autocomplete

| Keybind | Behavior |
| --- | --- |
| Enter | Send message (when input is valid and autocomplete did not consume Enter) |
| Tab | Accept/cycle autocomplete suggestion |
| Up / Down | Navigate autocomplete suggestions; if no autocomplete is active, navigate message history |

Notes:

- Autocomplete supports emotes, usernames, slash commands, and IRC /join channel suggestions.
- Bare-word Tab completion cycles emote/username matches.
- History starts with the newest sent message. Pressing Down past it restores
  the draft you were writing.
- Autocomplete and history keys affect the focused chat input.
- The Twitch character count and message limit behavior depend on the configured overflow mode.
- Pasting an image from the clipboard or dropping an image file onto the chat input triggers the configured uploader; the returned URL is inserted at the caret.

## Common mouse and context actions

- Right-click message row: reply/copy/moderation/workflow actions
- Right-click message row (mod): quick delete/timeout/ban/warn + full moderation submenus
- Right-click chat input: cut/copy/paste/select all/send now/clear input/insert /help/spell suggestions (top 5) + "Add to dictionary"
- Right-click channel tab: hide-when-offline toggle, Open in Streamlink
- Click username: open user card
- Click toolbar buttons: open settings, join dialog, analytics, whispers, IRC status, moderation tools
- Paste image (Ctrl+V) or drop file on chat input: upload to configured host and insert returned URL

## Built-in slash commands

These commands are handled by the UI parser. When supported, unknown commands
are passed to the chat server.

### General/local

- /help
- /clearmessages
- /reloadplugins
- /pluginsreload
- /chatters
- /fakemsg <text>
- /openurl <url>
- /logs open the Crust log/data folder in the system file manager
- /live list currently-live tracked Twitch channels

### Polls and predictions

- /poll
- /endpoll
- /cancelpoll
- /vote
- /prediction
- /lockprediction
- /endprediction
- /cancelprediction

### Creator and channel tools

- /commercial
- /marker
- /announce
- /shoutout
- /requests
- /setgame <category> update the Twitch stream category (broadcaster only)
- /settitle <title> update the Twitch stream title (broadcaster only)

### Moderation and safety

- /modtools
- /lowtrust
- /unbanrequests
- /resolveunban
- /automod
- /warn
- /monitor
- /restrict
- /unmonitor
- /unrestrict
- /banid
- /untimeout
- /shield <on|off> toggle Twitch Shield Mode (mod/broadcaster only)

### Chat and whisper helpers

- /redeem
- /reward
- /w
- /whisper

### Profile and external links

- /popout
- /user
- /usercard
- /streamlink [channel] open the given (or current) Twitch channel in Streamlink using the configured path / quality / extra args
- /follow-age [user] report how long a user has followed this channel (alias: /followage)
- /account-age [user] report the Twitch account age for a user (alias: /accountage)

### User-defined aliases

User-defined command aliases (Settings -> Commands) dispatch after the built-in table. Aliases support the variables `{1}`, `{1+}`, `{channel}`, `{user}`, `{input}`, and `{streamer}`, and recursive alias chains are rejected at save time.

### IRC-focused commands

- /nick
- /server
- /connect
- /join
- /part

### Plugin commands

- Plugin-registered slash commands are supported and participate in autocomplete.

## Related docs

- [Crust docs home](HOME.md)
- [Chat and channels API](API_Chat.md)
- [Moderation and creator API](API_Moderation.md)
- [Accounts and settings API](API_Accounts_Settings.md)
- [Core and callbacks API](API_Core.md)
- [UI API](API_UI.md)
- [Events API](API_Events.md)
- [Plugin API reference](API.md)
- [Release notes v0.5.1](releases/Release_v0.5.1.md)
- [Release notes v0.5.0](releases/Release_v0.5.0.md)
- [Release notes v0.4.9](releases/Release_v0.4.9.md)
- [Release notes v0.4.3](releases/Release_v0.4.3.md)

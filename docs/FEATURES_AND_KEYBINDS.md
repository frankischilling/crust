# Features And Keybinds

Last updated: 2026-04-22 (v0.5.0)

This page is a practical reference for what crust can do today: major features, keyboard shortcuts, high-use mouse actions, and built-in slash commands.

## Feature Overview

### Platforms and transports

- Twitch chat via IRC over WebSocket (anonymous and authenticated)
- Kick integration (beta, incomplete)
- Generic IRC integration (beta)
- IRC server tabs and keyed IRC joins
- IRC channel redirect handling

### Channels, tabs, and layouts

- Join, leave, reorder, and close channel tabs
- Sidebar layout and top-tabs layout
- Compact and normal tab styles
- Optional tab close buttons and live indicators
- Channel unread and mention counters
- Quick switch palette with unified channels + whisper threads
- Mention-first quick switch ordering (mentions, then unread, then others)
- Split-pane view (up to 4 panes)
- Tab visibility rules: right-click any Twitch tab to "Hide when offline"; tab reappears automatically when the stream goes live
- Live / followed-channels feed tab (aggregated list of every followed channel currently live)
- Persistent cross-channel Mentions tab (ring buffer, restored from local log on startup)

### Messages and rendering

- Chat, action, sub, raid, bits, channel points, timeout/ban/clear/system message rendering
- Reply/thread metadata and reply flow
- Span rendering for text, emotes, badges, mentions, URLs, and emoji
- URL detection and link preview metadata (Open Graph / Twitter card)
- Message deletion and user-message clearing states
- Optional timestamps (12h/24h, optional seconds)
- Optional long-message collapse

### Emotes, badges, and cosmetics

- Providers: Twitch, BTTV, FFZ, 7TV, Kick
- Animated emote support (GIF/WebP)
- Provider-priority resolution
- Badge rendering (global/channel sources)
- 7TV cosmetics updates (including name paint data)
- Emote picker with favorites, recents, and provider boost preferences

### Whispers

- Twitch whisper receive/send flow
- Threaded whisper list with recency ordering
- Per-thread unread counters and mention counters
- Whisper compose/open workflow
- Whisper notifications (when enabled)

### Moderation and creator tools

- Message-row quick actions: Quick Delete, Quick Timeout 10m, Quick Ban, Quick Warn
- Full moderation actions: timeout, ban, unban, warn, suspicious-user controls
- AutoMod queue approve/deny
- Unban request fetch/resolve
- Moderation presets
- Moderation tools window and workflow links
- Creator tools: polls, predictions, commercials, stream markers, announcements, shoutouts
- Reward redemption status updates

### Accounts, auth, and settings

- Multi-account add/switch/remove/default
- Auth refresh path
- Keyring-backed token storage with fallback
- IRC identity settings (nick and NickServ auth)
- Persisted settings for appearance, behavior, notifications, rules, filters, presets, and beta flags
- Remappable keyboard shortcuts: every binding routes through a central hotkey registry; rebind from Settings → Hotkeys with conflict detection
- Custom command aliases: user-defined `/trigger = body` pairs with `{1}`, `{1+}`, `{channel}`, `{user}`, `{input}`, `{streamer}` variables
- Spellcheck with personal dictionary: underlines misspelled words in the chat input, right-click for top 5 suggestions or "Add to dictionary"
- Sound event system: per-event ping sounds for mention / whisper / subscribe / raid / highlight, with per-event volume and streamer-mode muting
- Image uploader: clipboard paste or drag-drop to a configurable host (Imgur, Nuuls, ShareX SXCU, or any custom multipart endpoint); deletion URLs logged locally
- External tools: Streamlink integration with configurable path / quality / extra args, plus a custom player template (`{channel}`)
- Crash handler: structured panic reports with backtrace, tracing log tail, and live settings snapshot; abnormal-shutdown detection via session sentinels; in-app crash viewer on next launch

### Search, history, and local data

- Per-channel message search/filter UI
- Join-time history fetch
- Older local history paging
- Local append-only chat log indexing (SQLite)
- App data under platform-specific config/cache/log directories

### Plugin platform (Lua)

- Plugin runtime and lifecycle
- Plugin command registration
- Event callback registration
- Plugin UI surfaces: custom windows, settings pages, host panels
- Host callbacks for image/profile/link preview/IVR-log fetch workflows

### Updater

- Windows auto-updater via GitHub Releases
- Stable-only checks, SHA256 verification, staged install prompts
- Background/manual update checks
- Version skip support

## Keybinds

All shortcuts below are defaults. Every binding can be remapped from Settings → Hotkeys; conflicts are flagged inline.

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
- Twitch character count and over-limit behavior depends on the configured overflow mode.
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

This is the built-in command surface in the UI parser. Unknown commands are passed through to the backend/server path when applicable.

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

User-defined command aliases (Settings → Commands) dispatch after the built-in table. Aliases support the variables `{1}`, `{1+}`, `{channel}`, `{user}`, `{input}`, and `{streamer}`, and recursive alias chains are rejected at save time.

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
- [Release notes v0.5.0](releases/Release_v0.5.0.md)
- [Release notes v0.4.9](releases/Release_v0.4.9.md)
- [Release notes v0.4.3](releases/Release_v0.4.3.md)

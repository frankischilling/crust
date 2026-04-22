# Features And Keybinds

Last updated: 2026-04-06 (v0.4.3)

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

### Global and navigation

| Keybind | Behavior |
| --- | --- |
| Ctrl+K | Open quick switch palette |
| Ctrl+F | Open message search on active channel/pane |
| Escape | Close quick switch or close search (when open) |
| Ctrl+Tab, Ctrl+PageDown, Alt+Right | Next channel (prioritizes mentions/unread) |
| Ctrl+Shift+Tab, Ctrl+PageUp, Alt+Left | Previous channel (prioritizes mentions/unread) |
| Ctrl+1 .. Ctrl+9 | Jump directly to channel tab index 1..9 |
| Ctrl+Home | Focus first channel (or first split in split mode) |
| Ctrl+End | Focus last channel (or last split in split mode) |
| Alt+Shift+Left | Move active channel tab left (or move focused split left in split mode) |
| Alt+Shift+Right | Move active channel tab right (or move focused split right in split mode) |

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

## Common mouse and context actions

- Right-click message row: reply/copy/moderation/workflow actions
- Right-click message row (mod): quick delete/timeout/ban/warn + full moderation submenus
- Right-click chat input: cut/copy/paste/select all/send now/clear input/insert /help/spell suggestions
- Click username: open user card
- Click toolbar buttons: open settings, join dialog, analytics, whispers, IRC status, moderation tools

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
- /logs — open the Crust log/data folder in the system file manager
- /live — list currently-live tracked Twitch channels

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
- /setgame <category> — update the Twitch stream category (broadcaster only)
- /settitle <title> — update the Twitch stream title (broadcaster only)

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
- /shield <on|off> — toggle Twitch Shield Mode (mod/broadcaster only)

### Chat and whisper helpers

- /redeem
- /reward
- /w
- /whisper

### Profile and external links

- /popout
- /user
- /usercard
- /streamlink
- /follow-age [user] — report how long a user has followed this channel (alias: /followage)
- /account-age [user] — report the Twitch account age for a user (alias: /accountage)

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
- [Release notes v0.4.3](Release_v0.4.3.md)

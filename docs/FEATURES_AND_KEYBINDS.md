# Features and keybinds

This reference covers chat features, default shortcuts, mouse actions, and slash
commands. Kick and generic IRC support remain beta features.

## Feature overview

### Platforms and transports

- Twitch chat via IRC over WebSocket (anonymous and authenticated modes)
- Twitch EventSub events: subs / gift subs / raids / channel-points redemptions / poll + prediction lifecycle / stream `online` + `offline` / hype-train begin/progress/end / moderator + AutoMod + suspicious-user + whisper topics, with `session_reconnect` continuity, deduplication across 10,000 recent event IDs, and backfill after reconnect
- Kick chat integration (beta / currently incomplete)
- Generic IRC integration (beta), including server tabs, keyed channel joins, and private conversations
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
  - keyboard navigation that scrolls selection into view and follows search focus
- Nested chat layouts (up to 32 panes):
  - drag channel to split
  - split the focused view to the right or below, then resize either divider
  - focused-pane tracking
  - a search query and saved-filter selection for each pane, including duplicate views of one channel
  - an options menu in narrow panes, also available by right-clicking the header title
  - split keyboard navigation and pane reordering shortcuts
  - independent drafts and stable composer focus when panes are added, moved, or closed
  - independent scrolling and pause state when the same channel appears in two panes
  - reopen recently closed panes with their drafts, search, filters, and previous position
  - saved divider positions, focus, search fields, and filter selections restored at startup

### Detached chats and overlays

Open the chat header's options menu, then choose Open in new window or Open
overlay. The flat menu fits its action labels within the available window space.
Up to 16 chat windows can be open. Each has its own composer, reply target,
search, filters, emote picker, and scroll position.

Returning to the composer after using its emote picker or search keeps the same
native window. Native chat and search windows initialize their own accessibility
support.

Window size, position, search, filters, and overlay settings survive a restart.
Drafts and recently closed windows last for the session. Closing a channel's main
tab keeps its detached chats connected; closing the last view leaves the channel.

Choose Manage chat windows from the same header menu to show or close a window,
reopen a recently closed one, or restore its main tab. Reset position brings an
offscreen window beside the main window. Overlays have a background-opacity
slider labeled Background and an explicit Click-through mode. Use Interact in
the manager to restore mouse and keyboard control of an overlay.

### Desktop UI shell

- Join dialog and account/login switcher dialogs
- User profile popup and dedicated moderation tools window
- Whisper management window
- Optional analytics side panel
- Optional IRC status diagnostics panel
- Optional performance overlay for chat rendering stats
- Searchable settings with per-page scroll positions and responsive rule editors
- Account removal confirmation and a scrolling account list with a visible Close control
- Compact startup status while chat, settings, and navigation remain available
- Closing a pending startup channel removes it from the loading status
- Icon controls with visible keyboard focus, accessible button actions and labels,
  and the current state of toggles
- Resume scrolling supports Tab focus, Enter, and Space to return to live chat
- Layout controls that show the active layout and disable the sidebar when the
  window is too narrow
- Crash reports and IRC status remain within small windows. Failed report deletions
  stay available for retry; restarting from the report dialog saves through the
  normal shutdown path.

### Messages and rendering

- Message types rendered in chat include:
  - standard chat and actions
  - announcements with sender details, badges, emotes, and the notice color
  - subscriptions, raids, bits, channel points redemptions
  - hype train begin/progress/end with progress bar + level + top contributor (`channel.hype_train.*` EventSub)
  - shared chat (cross-channel mirror) with source-channel chip, source mod/vip badges, and notification suppression for already-open source channels
  - timeout/ban/clear notices and system notices
  - first-message and pinned-message indicators
- Inline replies with parent previews and jump navigation
- Rich span/token rendering:
  - text
  - emotes
  - badges
  - mentions
  - URLs
  - emoji
- URL detection and Open Graph/Twitter card preview metadata
- Message deletions, user clears, and channel clears update chat and Mentions,
  including saved and delayed history
- Sent-message confirmations retain server metadata and the local message identity;
  Shared Chat mirrors remain separate from sends in the current channel
- Room modes, raid notices, Hype Trains, and Shared Chat banners fit narrow panes
- Optional timestamp rendering (12h/24h, with/without seconds)
- Optional long-message collapse with configurable line limits
- Unicode-aware tokenization that preserves whitespace and ignores invalid or
  overlapping native-emote ranges

### Input and command UX

- Message input history recall (arrow keys)
- Rebindable input actions in Settings > Hotkeys > Chat input, including Send,
  Send and keep text, completion, history, Clear input, Cancel reply, and the emote picker
- Case-insensitive input-history search with a query, match preview, and older/newer navigation
- Emote picker access in narrow panes, with the character count below the input when needed
- Rejected sends keep their original draft and reply context for recovery
- Channel changes retain each view's unsent draft during the session. Delayed
  send failures and upload results return to the originating composer when it
  is available; otherwise, they remain in unsent recovery.
- `:` emote autocomplete and Tab completion
- Scrollable completion menus that fit short windows and enlarged fonts, with
  pointer selection that returns focus to the composer
- Slash command handling for chat, moderation, and creator workflows
- Command usage tracking for ranking/autocomplete improvements
- Custom command aliases (`/hi = /me says hi {1}`) with `{channel}`, `{user}`, `{input}`, `{streamer}` variables
- Spellcheck on the input box (Norvig + Soundex + QWERTY proximity), with up to five right-click suggestions and a persistent personal dictionary
- Image paste / drag-drop uploader (Imgur, Nuuls, ShareX SXCU, or any custom multipart endpoint), with deletion URLs logged locally

### Reply threads

- Open thread from a Twitch message's context menu in chat, search, or user history
- View retained replies with the normal emotes, badges, links, and message actions
- Compose a reply inside the thread while keeping the main chat draft intact
- Subscribe to a thread to include new replies in the existing highlight and
  Mentions flow. Live participation subscribes automatically; an explicit
  unsubscribe remains in effect for that retained subscription entry.
- Thread drafts and subscriptions belong to the active account and last for the
  session. Up to 64 thread views and 256 subscription choices are retained.
- When the original message is unavailable, hover over Original message not
  loaded to inspect the available quote context. Opening a thread uses retained
  channel history; it does not fetch a complete archive of older replies.

### Emotes, badges, and cosmetics

- Emote providers:
  - Twitch native
  - BTTV
  - FFZ
  - 7TV
  - Kick
- Full native global emote catalog for signed-in Twitch accounts, without the
  optional `user:read:emotes` permission; bundled globals remain available while it loads
- Channel emotes stay in their channel; personal emotes resolve for their owner
- Paginated Twitch account inventory for the picker, autocomplete, and outgoing
  emote ranges, with the optional `user:read:emotes` scope
- Follower emotes restricted to their channel and the account that can use them
- Provider and scope precedence is consistent between chat, autocomplete, and the picker
- Static PNG/JPEG and animated GIF/WebP decoding use a bounded background queue
- Animated emotes normalize missing or tiny frame delays so playback keeps advancing
- Layered 7TV and BTTV global overlay emotes in chat and whispers, including live 7TV flag changes
- Emoji tokenization/rendering via Twemoji URLs
- Badge rendering from global/channel badge sources
- 7TV sender colors, selected username paints, badges, and avatars
- Chat settings for showing 7TV name paints, paint shadows, and larger shadows
- Emote picker features:
  - favorites
  - recents
  - provider order preferences
  - focused search, Ctrl+Tab / Ctrl+Shift+Tab navigation, and insertion at the caret
  - favorites menus that stay open while the pointer moves from an emote to an action
- Emote image caching in memory and on disk
- Images load as they become visible; provider catalogs become available independently
- Live channel-set additions, renames, and removals from BTTV and 7TV on Twitch,
  with 7TV channel-set updates also available on Kick
- Active 7TV channel-set changes and reconnect reconciliation, with stale
  subscription and snapshot checks
- Live global and one selected personal 7TV set per sender, with ownership and membership refreshes
- Coalesced channel refreshes and five-minute BTTV, FFZ, and 7TV global metadata
  refreshes that retain usable catalogs on failure
- Automatic retries for failed channel providers after a 30-second cooldown,
  with successful catalogs and valid empty results preserved
- Smaller 7TV assets for ordinary rendering, with larger images kept for HD previews

Chat and native global emotes work without `user:read:emotes`. Personal inventory
and follower-emote permissions still use that scope. FFZ uses metadata refreshes. The
[UI and performance audit](AUDIT_UI_PERFORMANCE.md) explains provider limits,
cached-snapshot behavior, and the measured rendering workloads.

Settings > Chat > 7TV name paints has separate controls for paints, shadows, and
larger shadow blur. Disabling paints or shadows preserves the choices beneath
that control. Paint animation follows the existing window-focus animation setting.
Known sender paints appear in channel chat, Mentions, search results, reply
previews, inline Twitch @mentions, and received-whisper headers. Unknown names
keep readable plain text while their cosmetics are unavailable.

### Whispers

- Twitch whisper sending and receiving
- Whisper thread list with recency ordering
- Per-thread unread counts and mention counts
- Whisper compose/open workflow from thread context
- Whisper desktop notifications (when enabled)
- Received sender names use their current 7TV paint when known
- Saved threads belong to the account's immutable Twitch user ID and remain
  available after that account changes its login
- Legacy rows with no owner or only a login-based owner stay stored but are not
  assigned to a current account
- Sender IDs survive saved history, allowing personal emotes to resolve for their owner
- Reply opens the whisper composer without requiring a joined channel
- Per-recipient drafts remain available until a successful send response;
  failed attempts can be recovered without replacing newer text
- Temporary send-failure notices visible only to the owning account, cleared on
  account changes or logout

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
  - shortcuts require the active, expanded window; text editors keep ordinary
    typing and Tab behavior, and navigation follows the filtered queue
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
- Word-boundary mention matching and channel-scoped nickname aliases
- Tab visibility rules ("Hide when offline" per-channel)
- Desktop notifications for mentions/highlights/whispers (configurable)
- In-app event toast banners for high-visibility events
- Per-event sound notifications (mention / whisper / subscribe / raid / custom highlight) with file-path + volume sliders, baked-in synthetic ping fallback, and streamer-mode mute integration

### Accounts, auth, and settings

- Multi-account support:
  - add/switch/remove accounts
  - set default startup account
- Auth refresh flow for expired/invalid sessions
- Tokens saved in the settings file, with OS keyring storage when available
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

Open a chat header's options menu and choose Pane filters to select the saved
rules for that view. Use all enabled filters follows the global settings;
clearing it allows a subset or no rules. Disabled rules remain inactive. The
selection uses saved rule names, so a renamed or deleted rule appears as not
found until you update the selection.

### Search, history, and local data

- Recent-message history load on join (recent-messages API with IVR fallback)
- Per-channel local message logging and indexing (SQLite)
- Ordered background log writes and an indexed Mentions query
- Older local history paging, including messages with identical timestamps and rows without server IDs
- Per-channel message search/filter UI
- Local search fields fit the popup width. Help and regex errors wrap, and a
  scrollbar appears only when the form is taller than the window.
- Cross-channel search with predicates such as `from:<user>`, `has:link`, `in:<channel>`, `is:announcement`, and `regex:"<pattern>"`
- Saved expression filters can match announcements with `flags.announcement`.
- Search navigation that follows input/result focus and leaves other controls' keys alone
- Search results and recent messages in user cards use the chat renderer for
  emotes, badges, replies, subscription messages, links, and message actions
- Opening a message, reply, thread, or user card from cross-channel search closes
  its window and returns focus to chat. Reopening search in the same session
  restores the query.
- Case-insensitive search regexes by default; inline regex flags can request case-sensitive matching
- Subscriber-tier searches distinguish tier from subscription length; comma-separated `is:` values match any listed flag
- Data directories via `directories::ProjectDirs` for config/cache/logs

Local history loads in pages. Search can request older logs even when the current
buffer has no matches, and shows "No older logs" after the saved history is
exhausted. Explicit older-history loads can expand a channel buffer to 50,000
messages; ordinary live chat keeps its 1,500-message limit.

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
  - structured panic reports (version, target triple, run id, OS / arch / display server, pid, thread, payload, force-captured backtrace, last ~512 tracing events, live settings summary)
  - settings summaries exclude credentials and free-form settings such as commands, URLs, and headers
  - synthetic "abnormal exit" reports on SIGKILL / power loss / native crash detected via session sentinel
  - rolling 20-report + 40-sentinel retention
  - in-app crash viewer with View / Copy / Show on disk / Delete / Dismiss / Restart actions

## Keybinds

The tables below show default shortcuts. Settings > Hotkeys lets you rebind
registered app actions, including chat input, and reports conflicts in overlapping
contexts. Palette navigation and ordinary text editing keep their widget controls.

While a shortcut is waiting to be rebound, keyboard input goes to that binding.
Escape cancels without changing it. Leaving Hotkeys or closing Settings also
cancels an unfinished capture.

In Settings, Escape clears a nonempty search or dismisses the active menu or
expression editor first. A subsequent Escape closes the active Settings window.

### Global and navigation

| Keybind | Behavior |
| --- | --- |
| Ctrl+K | Open quick switch palette |
| Ctrl+F | Open message search on active channel/pane |
| Ctrl+Shift+F | Open cross-channel search popup (predicate DSL) |
| Ctrl+, | Open settings |
| Ctrl+W | Close the focused chat window, split, or channel tab |
| Escape | Close quick switch, focused search controls, or the active Settings window after its child controls |
| Ctrl+Tab, Ctrl+PageDown, Alt+Right | Next channel (prioritizes mentions/unread) |
| Ctrl+Shift+Tab, Ctrl+PageUp, Alt+Left | Previous channel (prioritizes mentions/unread) |
| Ctrl+1 .. Ctrl+9 | Jump directly to channel tab index 1..9 |
| Ctrl+Home | Focus first channel (or first split in split mode) |
| Ctrl+End | Focus last channel (or last split in split mode) |
| Alt+Shift+Left | Move active channel tab left (or move focused split left in split mode) |
| Alt+Shift+Right | Move active channel tab right (or move focused split right in split mode) |
| Ctrl+= | Increase chat font size |
| Ctrl+- | Decrease chat font size |
| Ctrl+0 | Reset chat font size |

In a detached chat, Ctrl+F searches that window. Ctrl+K, Ctrl+Shift+F, and Ctrl+,
bring the main window forward for quick switch, cross-channel search, and settings.
Ctrl+W closes only the detached window that has focus.

### Chat panes

| Keybind | Behavior |
| --- | --- |
| Ctrl+Alt+R | Split the focused chat to the right |
| Ctrl+Alt+D | Split the focused chat below |
| Ctrl+Shift+T | Reopen the most recently closed pane |
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
| Enter | Send a valid message when no input helper is using Enter |
| Shift+Enter | Send without accepting the open completion menu |
| Ctrl+Enter / Ctrl+Shift+Enter | Send and keep the text in the composer |
| Tab | Accept/cycle autocomplete suggestion |
| Shift+Tab | Cycle autocomplete suggestions backward |
| Up / Down | Navigate autocomplete suggestions; if no autocomplete is active, navigate message history |
| Ctrl+R | Open input-history search; while searching, select an older match |
| Ctrl+Shift+R | Open input-history search in the forward direction; while searching, select a newer match |
| Ctrl+U | Clear the focused input |
| Escape | Dismiss the active completion or history helper; otherwise cancel the pending reply |
| Ctrl+E | Open the emote picker for the focused composer |

Notes:

- Autocomplete supports emotes, usernames, slash commands, and IRC /join channel suggestions.
- Completion replaces the word at the caret and preserves the text after it.
- Tab cycles emote and username matches; Shift+Tab reverses the cycle.
- History starts with the newest sent message. Pressing Down past it restores
  the draft you were writing.
- Input-history search matches text without case sensitivity and previews the
  selected sent message. Up and Down move between matches and wrap at either end.
  Enter or Tab puts the selected message into the composer for editing. Escape
  or Cancel closes the search and keeps the original draft. Searching and
  accepting a result never send a message.
- Autocomplete and history keys affect the focused chat input.
- Input events keep their order around Send, Clear input, and pane navigation.
  Text typed after Send begins the next draft, including when both events arrive
  in one UI update.
- Rebinding or clearing Send removes its default Shift+Enter alternative too.
  The same rule applies to Send and keep text and Ctrl+Shift+Enter. Settings
  displays both alternatives while their default Enter binding is active.
- An input-history query belongs to its composer and closes when that composer
  changes channels. Search uses the same retained messages as Up/Down recall.
- A rejected send keeps the draft and reply. If a later runtime rejection arrives
  after a new draft was started, the unsent message remains available to copy or
  restore without replacing that draft.
  Choosing Restore brings the original composer into view, including a closed
  pane or detached window. A late failure does not change the view on its own.
- The Twitch character count and message limit behavior depend on the configured overflow mode.
- Pasting or dropping an image onto a composer starts the configured uploader.
  Its URL returns to that exact pane or reply thread, including an inactive
  channel draft. If the destination is unavailable or the account has changed,
  the link is retained with unsent messages for the original account.

## Common mouse and context actions

- Right-click message row: reply/open thread/copy/moderation/workflow actions
- Right-click message row (mod): quick delete/timeout/ban/warn + full moderation submenus
- Right-click chat input: cut/copy/paste/select all/search input history/send now/clear input/insert /help/spell suggestions (top 5) + "Add to dictionary"
- Click split options or right-click its title: split right or below, reopen a
  closed pane, choose filters, show/hide message search, close that split, or close
  the other splits. These actions remain available in narrow panes.
- Right-click channel tab: hide-when-offline toggle, Open in Streamlink
- Click a sender name in channel chat or a Twitch @mention: open that user's card
- Click the body of a Mentions or search result: navigate to its message;
  sender names open user cards and links retain their own actions
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
- [Release notes v0.7.2](releases/Release_v0.7.2.md)

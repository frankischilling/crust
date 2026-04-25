# Crust v0.5.1 Release Notes

Date: 2026-04-24

v0.5.1 closes out a wide swath of subsystems: full Twitch EventSub coverage, a typed filter expression DSL, hype-train + raid banners above the chat list, first-class Shared Chat mirrors, embedded-webview channel-points bonus auto-claim (a Crust-only feature, not present in other native Twitch clients), and a new plugin API surface for filters / sound / hotkeys / uploader. Layered on top: a settings-persistence sweep so every setting actually round-trips to disk, ten independent font-size knobs in Settings, and a round of emote-pipeline fixes.

## Highlights

### New subsystems

- **Twitch EventSub coverage closed.** The subscription registry in `crates/twitch/src/eventsub/{registry,session,parser,notice}.rs` now covers subs / gift subs / raids / channel-points redemptions / poll + prediction lifecycle / stream `online` + `offline` / hype-train begin/progress/end / moderator topics / AutoMod / suspicious-user / whispers. Reconnect uses Twitch's `session_reconnect` URL so the same session id continues, a 10 000-entry event-id LRU deduplicates replays across reconnects, and a per-reconnect backfill refreshes live-status profiles + moderated-channel unban queues so events older than ~1 minute aren't lost.
- **Typed filter expression DSL.** `crates/core/src/filters/{lexer,parser,ast,types,eval,context}.rs` ships a complete filter language: lexer -> parser -> AST -> static type checker -> evaluator. `FilterRecord` and `HighlightRule` both gained an `Expression` mode alongside the existing substring / regex modes, and legacy data loads unchanged.
  - Identifiers cover `author.*`, `channel.*`, `message.*`, `flags.*`, `has.*`, `reward.*` (full table in `CHATTERINO_PARITY_TODO.md` appendix).
  - Operators: `&&`, `||`, `!`, `==`/`!=`/`<`/`<=`/`>`/`>=`, `contains` / `startswith` / `endswith` / `match`, `+`/`-`/`*`/`/`/`%`. `match` takes a regex literal (`r"..."` or `ri"..."` for case-insensitive); arithmetic is integer-only and divide-by-zero returns `0` rather than panicking.
  - `crates/ui/src/widgets/filter_editor.rs` hosts a shared inline mode cycler (`Aa` / `.*` / `ƒx`) and an "Advanced expression editor" modal with live parse feedback and an identifier / operator palette. Errors carry a `Span` so the UI can point at the exact line and column.
- **Hype train + raid banners.** New `MsgKind::HypeTrain { phase, train_id, level, progress, goal, total, top_contributor_*, ends_at }` plus a `MsgKind::Raid.source_login` field. `crates/ui/src/widgets/hype_train_banner.rs` paints a progress bar, level badge, total points and top contributor above the message list, updating from `channel.hype_train.{begin,progress,end}` EventSub topics (subscribed when the broadcaster holds `channel:read:hype_train`). Raid arrivals show a dismissible banner with raider count + source login. Both run in split-pane and single-pane layouts.
- **Auto-claim of channel-points Bonus Points (Crust-only).** Native Twitch chat clients don't ship this. Crust embeds a WebView2 instance (via `wry`) that signs into twitch.tv with the configured session token and DOM-clicks the Bonus Points button every 30 s when the toggle is on. Balance polling continues through the existing GraphQL `channel_points_claimer`. Toggle: `auto_claim_bonus_points` under Settings > Channel points. Setup notes: `docs/superpowers/specs/2026-04-24-twitch-webview-auto-claim-setup.md`.
- **Shared Chat (cross-channel mirror).** Mirrored Twitch PRIVMSGs (Shared Chat sessions) now show an `↗ <source>` chip in source-channel colours with a "Shared Chat | Mirrored from #<source>" tooltip, and source-channel mod / vip badges append with a "(from <channel>)" suffix. The reducer flips `MessageFlags::suppress_notification` when the source channel is also open, so sound + desktop toast + mentions-feed all skip the duplicate. `sharedchatnotice` USERNOTICE msg-ids are remapped via `source-msg-id` and non-announcement mirrors are dropped so sub counts stay accurate. Eight unit tests in `client.rs::shared_chat_tests` cover tag parsing and the three USERNOTICE classifications.
- **Usercard depth.** The user profile popup (`crates/ui/src/widgets/user_profile_popup.rs`) gains follow-age, account-age, and a shared-channels list driven by Helix follow / user-creation lookups (cached, with a "not following" fallback). The popup aggregates other open channels where the user has typed recently, and the moderator view surfaces prior timeouts for that user in the current session.
- **AutoMod + low-trust + unban request queue UI.** The Moderation Tools window (`render_mod_tools_window` in `app.rs`) now hosts three dedicated tabs (`AutoMod`, `LowTrust`, `UnbanRequests`) with per-tab item-count badges, a shared filter strip (login or message text), per-row approve / deny actions, bulk approve and bulk deny, an unban request "Refresh" button, and full keyboard control: `J` / `K` focus next / prev, `A` / `D` approve / deny focused, `Shift+A` / `Shift+D` bulk approve / bulk deny, `Tab` / `Shift+Tab` switch tabs. Hotkeys are routed through the existing `HotkeyAction` registry so they show up in Settings > Hotkeys and can be rebound.

### Plugin API surface expansion

The `c2` Lua surface picks up bridges for the new subsystems plus expanded snapshot accessors:

- `c2.filters_parse(expr)` / `c2.filters_evaluate(expr, ctx)` for the typed filter language.
- `c2.upload_image(channel, bytes_b64, format, source_path?)` plus `UploadStarted` / `UploadFinished` callback events.
- `c2.set_sound_settings(events)` / `c2.get_sound_settings()` and `SoundSettingsUpdated` events.
- `c2.set_hotkey_bindings(bindings)` / `c2.get_hotkey_bindings()` and `HotkeyBindingsUpdated` events.
- Demo plugin under `plugins/c9_api_expansion_demo/` exercises every surface.

### Bug fixes

- **Settings persistence (full sweep).** Several settings were silently failing to round-trip across restarts because their writers updated the in-memory snapshot without invoking `AppSettings::save()`. Every `AppCommand::Set*` handler in `crates/app/src/main.rs` now drives the same persist path, and `crust-storage::save()` re-fires the persist hook so the crash-reporter snapshot stays in sync with disk. Affected categories include font sizes, theme, sidebar / analytics / IRC visibility, tab style, hotkeys, sound events, ignores / phrases, command aliases, tab visibility rules, filter records, highlight rules, and uploader / external-tools config.
- **Emote rendering.** Several fixes for the emote pipeline:
  - Provider-aware resolution no longer resurrects a stale BTTV / FFZ / 7TV catalogue entry after a channel-emote refetch supersedes it.
  - Animated GIF / WebP frames decode against the catalogue entry that requested them, so a re-load of the same shortcode no longer paints the previous frame on top.
  - Emote picker recents and favourites are deduplicated by canonical URL before persistence, which stops an emote from appearing multiple times after provider catalogues swap.
  - Inline 7TV link rendering (channel-points reward posts that ship a `7tv.app/emotes/<id>` URL) is hardened against `www.7tv.app` and `http://` variants.

### New features

- **Expanded font controls.** Settings > Appearance now exposes ten distinct font-size knobs instead of just chat body + UI scale:
  - `font_size`: chat body
  - `ui_font_size`: egui `pixels_per_point` ratio
  - `topbar_font_size`: top chrome toolbar labels
  - `tabs_font_size`: channel-tab chip labels
  - `timestamps_font_size`: message timestamps
  - `pills_font_size`: room-state / viewer-count pills
  - `popups_font_size`: tooltips and popovers (0.0 = auto-follow chat)
  - `chips_font_size`: inline chips and inline badges (0.0 = auto-follow chat)
  - `usercard_font_size`: usercard headings (0.0 = auto-follow chat)
  - `dialog_font_size`: login / setup dialog helper text (0.0 = auto-follow chat)
  Each knob has its own slider, a per-section reset, and a global "reset all" action. The runtime emits `AppEvent::FontSettingsUpdated` so plugin VMs see the same snapshot the host does, and `AppCommand::SetFontSizes` carries the full ten-field payload through the reducer.

## Settings additions

- `topbar_font_size: f32` (default 12.0)
- `tabs_font_size: f32` (default 12.0)
- `timestamps_font_size: f32` (default 11.0)
- `pills_font_size: f32` (default 11.0)
- `popups_font_size: f32` (default 0.0 = auto)
- `chips_font_size: f32` (default 0.0 = auto)
- `usercard_font_size: f32` (default 0.0 = auto)
- `dialog_font_size: f32` (default 0.0 = auto)
- `auto_claim_bonus_points: bool` (default false)
- Filter records and highlight rules pick up an `Expression` mode (existing rows continue to load as `Substring` / `Regex`).

All new fields ship with `#[serde(default = "...")]`, so existing `settings.toml` files pick them up on first load with no manual migration. The `0.0 = auto` convention on the four optional font sections keeps the previous derived sizing until the user opts in.

## Plugin API additions

- `c2.filters_parse(expr)` returns `{ ok = true, type = "Bool" }` or `{ ok = false, error = "<message>" }`.
- `c2.filters_evaluate(expr, context)` returns `bool` or `nil, error_message`.
- `c2.upload_image(channel, bytes_b64, format, source_path?)`.
- `c2.set_sound_settings(events)` / `c2.get_sound_settings()`.
- `c2.set_hotkey_bindings(bindings)` / `c2.get_hotkey_bindings()`.
- New events: `c2.EventType.SoundSettingsUpdated`, `UploadStarted`, `UploadFinished`, `HotkeyBindingsUpdated`.

Existing handlers that only read `chat_font_size` / `ui_font_size` from `FontSettingsUpdated` continue to work unchanged; the new fields are additive.

## Upgrade notes

- If a setting appeared to "reset on restart" before this release, that snapshot is now load-bearing, including any tweaks made in the last v0.5.0 session that persisted in-memory but were lost on exit. Re-set the affected fields once after upgrading.
- The emote picker dedupes recents on first launch; an entry that previously appeared twice will collapse to a single row.
- Auto-claim of Bonus Points ships disabled. Open Settings > Channel points and toggle it on; the embedded webview signs in via the existing Twitch session token.
- Filter and highlight expressions catch typos at save-time (positional `ParseError` / `TypeError`), so a previously-silent typo in a substring rule may surface a save-time error if the rule is migrated to `Expression` mode. Substring / regex rules continue to load and run unchanged.
- Hype train + raid banners render above the message list. If a custom layout assumed the message list started at panel top, expect ~28 px of banner height when an event is active.

## Notes

- Workspace and internal crate versions bumped to 0.5.1.
- New `crust-webview` and `crust-webview-host` workspace members back the embedded-webview auto-claim flow.
- `crust-uploader` continues from v0.5.0; v0.5.1 only touches its plugin-facing wrapper.
- IRC transport and crash-reporter behaviour from v0.5.0 carry forward unchanged.
- Local SQLite log format is unchanged; legacy rows without `shared` / `suppress_notification` deserialise via `#[serde(default)]`.

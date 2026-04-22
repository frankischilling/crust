# Crust v0.4.9 Release Notes

Date: 2026-04-22

v0.4.9 works on the Chatterino parity: nicknames, unified ignores, pronouns on user cards, cross-channel search with predicates, per-section font sizes, low-trust moderation surface, streamer mode, and a batch of new slash commands. It also lands rendering-performance work for long chat buffers.

## Highlights

### New features

- **Nicknames / user aliases (A1).** Per-user display-name overrides applied across chat rendering, user cards, and mention detection. Edited from a new "Nicknames" settings tab. Persists across restarts.
- **Unified ignores (A2).** Separate lists for ignored users (full-user block) and ignored phrases (plain or regex) with per-entry actions: block message, replace with `***`, highlight only, mention only. Regex validation surfaces invalid patterns inline. Ignored users do not render and do not trigger mention highlights.
- **Pronouns on user cards (A3).** Opt-in fetch from the alejo.io pronouns provider when a user card opens. In-memory caching to avoid repeat hits. Off by default.
- **Cross-channel search (A4).** Ctrl+F gains a "search all channels" scope plus a lightweight predicate DSL: `from:<user>`, `has:link`, `in:<channel>`, `regex:"<pattern>"`. Invalid predicates show inline errors. Keyboard nav between hits.
- **Font size + zoom controls (A5).** Separate persistent sizes for chat body, UI scale, top toolbar, tabs, timestamps, and room-state pills. Sliders in the Appearance settings tab; `Ctrl+=`, `Ctrl+-`, `Ctrl+0` adjust chat font on the focused split.
- **Low-trust moderation surface (A6).** `/monitor`, `/unmonitor`, `/restrict`, `/unrestrict` backed by the Helix suspicious-users endpoint. Messages from monitored/restricted users render with a visual badge.
- **Streamer mode (A7).** Detects running broadcasting software (OBS, Streamlabs, PRISM, XSplit, Twitch Studio, vMix) and, while active, hides link-preview tooltips, suppresses viewer counts in split headers, and silences sound notifications. Modes: `off`, `auto`, `on`. Per-feature toggles under the new "Streamer mode" settings tab.
- **Extra slash commands (A8).** Added `/logs`, `/live`, `/shield`, `/setgame`, `/settitle`, `/follow-age`, `/account-age`. Each is registered for autocomplete, documented in `docs/FEATURES_AND_KEYBINDS.md`, and either executes or reports a specific error for permission or argument issues.

### Performance

- **Hot-window rendering for long chat buffers.** Very long scrollback now renders through a sliding active window with a boundary marker so frame cost no longer scales linearly with buffer length when the user is following the tail.
- **Focus-gated animation loop.** Animated emote repainting runs only while the window is focused by default, cutting idle-background CPU.
- **Throttled chatter-list rebuilds.** Sorted-chatter recomputation is coalesced per channel and throttled so busy channels no longer pay the O(n log n) cost on every new-chatter event.
- **Incremental emote RAM accounting.** Running total of raw emote bytes is maintained on `EmoteImageReady` rather than walked every frame, removing a per-frame hash-map scan.

## Slash command reference

New in v0.4.9 (full catalogue in `docs/FEATURES_AND_KEYBINDS.md`):

- `/logs` - open the Crust log/data folder in the system file manager
- `/live` - list tracked Twitch channels currently live
- `/shield <on|off>` - toggle Twitch Shield Mode (mod/broadcaster only)
- `/setgame <category>` - update the Twitch stream category (broadcaster only)
- `/settitle <title>` - update the Twitch stream title (broadcaster only)
- `/follow-age [user]` - report how long a user has followed this channel (alias: `/followage`)
- `/account-age [user]` - report the Twitch account age for a user (alias: `/accountage`)

## Upgrade notes

- Settings file gains new fields (nicknames, ignored users, ignored phrases, pronouns toggle, per-section font sizes, streamer-mode block). Older configs pick up defaults automatically; no manual migration is required.
- Streamer mode defaults to `off`. Switching to `auto` enables a ~20s background poll for broadcasting software.
- Pronoun fetches stay off until you opt in under the User Cards section.
- Low-trust commands require moderator scope on the target channel.
- `/setgame` resolves a category name to a `game_id` via `GET /helix/games?name=...` before calling `PATCH /helix/channels`; a specific error is shown if the category cannot be found.

## Notes

- Workspace and internal crate versions bumped to 0.4.9.
- Streamer-mode detection uses `tasklist.exe` on Windows (no console flash via `CREATE_NO_WINDOW`) and `pgrep` on Linux/macOS.
- Cross-channel predicate parsing is permissive: unknown keys are treated as plain substring terms so legacy searches continue to work.
- EventSub and updater behavior from v0.4.8 carry forward unchanged.

# Crust v0.5.2

Date: 2026-11-14

This release fixes chat delivery, history paging, emote ownership, and keyboard
focus. It also reduces repeated work in highlights and paused chat, limits the
runtime image cache, and adds Linux build and package checks.

## Chat and history

- Twitch messages appear in chat after their socket write completes. Queue
  rejection, disconnects, and overlong messages preserve the original draft and
  reply for recovery. A completed write still cannot guarantee server acceptance;
  server notices can report a later rejection.
- When local logging is enabled, successful sends are saved to history. Failed
  whispers are no longer recorded as sent.
- Generic IRC echoes also wait for successful writes and use the nickname
  confirmed by the server. `/me` keeps its action formatting, and failed sends
  return to draft recovery.
- Older logs page by timestamp and row ID, so messages in the same millisecond
  are not skipped. Stable saved-row IDs prevent duplicates when loading a page
  again, including messages without server IDs.
- Loading older logs keeps those rows available for search. Search can request
  older history when the current buffer has no matches and stops offering loads
  once the saved history is exhausted.
- With local logging enabled, custom highlights survive a restart in Mentions.
  Search handles subscription tiers, punctuation, case-insensitive regexes, and comma-separated message flags
  correctly.

## Emotes and controls

- Channel emotes stay in their own channel, and personal emotes in channel chat
  resolve for the sender who owns them. Chat, autocomplete, and the picker use the
  same scope and provider precedence.
- Global provider sets load alongside channel sets. Replaced, removed, and
  changed-image entries update existing messages, including older saved history.
  Refresh work is spread across frames to keep large buffers responsive.
- The picker focuses search when opened, handles Ctrl+Tab and Ctrl+Shift+Tab,
  closes with Escape from search, keeps favorites menus open, and inserts at the
  caret or selection.
- Split panes retain their drafts and composer focus when added, reordered, or
  closed. The reply dismiss control remains reachable at narrow widths.
- Channel dragging follows the channel itself when filtering or live status
  changes the visible list.

## Performance and reliability

- Highlight regexes compile when rules change instead of for every message.
- Paused chat reuses its message-index mapping. Changing a hide filter can reveal
  messages from the paused snapshot without admitting new arrivals.
- The runtime image cache enforces both byte and entry limits. Disk reads stop
  at 32 MiB, and oversized entries are discarded.
- Generic IRC has bounded input lines, registration and write deadlines, and
  cancellable reconnect waits. Nick collisions and channel redirects update the
  connection's actual state.
- Fixed a Linux initialization failure that prevented the embedded sign-in
  window from opening. An offline startup check now verifies communication
  between the webview's JavaScript and its host process.
- Windows packaging stops on build errors. Debian packaging derives ABI
  requirements from both binaries and includes the dynamically loaded X11
  keyboard library. Both packages include portable SHA256 checksum files.

This release also includes the earlier November audit fixes for account changes,
connection recovery, moderation deduplication, image request backpressure,
settings layout, Unicode text, uploads, and plugin callbacks. The
[original audit](../AUDIT.md) and [follow-up audit](../AUDIT_FOLLOWUP.md) record the
details and validation limits.

## Upgrade and plugin notes

Existing settings and local logs load without a manual migration. Kick support
remains incomplete, including sending. Generic IRC remains a beta feature.

The Debian package is built on Ubuntu 24.04; its dependency metadata specifies
the required library versions. It requires WebKitGTK 4.1. See
[Building Crust](../BUILDING.md) to build on another distribution.

`c2.load_older_local_history` accepts an optional row cursor, and `HistoryLoaded`
exposes the next cursor and whether more saved rows exist. Emote catalog events
include owner and image metadata. Saved-message IDs are opaque and can appear as
negative integers in Lua. Details are in the [chat API](../API_Chat.md) and
[event reference](../API_Events.md).

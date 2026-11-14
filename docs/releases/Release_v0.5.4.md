# Crust v0.5.4

This release reorganizes settings, keeps the emote picker accessible in narrow
chat panes, and moves animation decoding off the UI thread. It also adds
channel-specific Twitch follower inventory and extends live 7TV updates to
global and personal sets.

## Interface

- Search settings by page or option. Connections, Plugins, External tools, and
  Updates have separate pages, with compact navigation in narrow windows.
- Settings pages keep their scroll positions and field focus when resized.
  Highlight and filter editors preserve their actions in narrow layouts.
- The expression editor keeps Save and Cancel visible, handles Unicode
  selections, and receives Escape before underlying chat popups.
- Accounts separates status from actions, keeps Close visible while scrolling,
  and confirms removal of a saved account. Closing it clears the token field.
- The toolbar uses compact action groups and clearer layout/settings icons.
- The composer keeps its emote button in narrow panes and moves the character
  count below the input when needed. Enter continues to submit valid drafts.
- The picker has a simpler header and provider-order menu. Unused chat emotes
  appear before unused emoji in All; explicit preferences retain their priority.
- Changing a split's channel keeps its draft and selection. Replies remain tied
  to their original channel.
- Analytics switches and exports the correct channel immediately. Reset is
  channel-specific and no longer panics. Late profile responses cannot reopen a
  dismissed user card.
- Search respects the active dialog's Escape key. Moderation shortcuts operate
  only in the active, expanded window and follow the visible filtered queue.
  Text fields retain their ordinary typing and Tab behavior.
- Whispers has its own recipient composer, including when no channel is open.
  Drafts remain available through send failures without replacing newer text.
  Results from an earlier account session cannot alter a new send attempt.
- User cards scroll within short windows, keeping long profiles, moderation
  controls, and the title-bar close action reachable.

## Emotes

- Follower emotes are available only for the signed-in account and their channel.
  They appear in the picker, autocomplete, chat, and outgoing emote ranges.
  Whispers and other channels do not inherit them.
- 7TV global and selected personal sets receive live changes, including personal
  membership changes and reconnect reconciliation.
- Stale cached metadata cannot restore a recently removed emote or retired set.
  Failed global refreshes preserve usable catalogs; valid empty inventories
  remain distinct from failures. Global metadata refreshes every five minutes.
- Repeated channel reloads coalesce. Four shared provider groups bound ordinary
  channel requests, and obsolete queued work exits before making requests.

Twitch inventory needs the optional `user:read:emotes` token scope. Chat works
without it. FFZ continues to use metadata refreshes.

## Responsiveness

- GIF/WebP animation frames decode on two background workers with a bounded
  queue, byte reservations, and stale-result checks.
- A seven-run release benchmark reduced the median first UI call for a
  120-frame GIF from 12.948 ms to 0.0415 ms. Full decoding remained about 13 ms.
- The Performance overlay separates UI work from idle repaint intervals.
  Detailed phase logs include event backlog, retained and visible history,
  decode work, and texture submission work.

The [audit and measurements](../AUDIT_UI_PERFORMANCE.md) describe the workloads,
regression coverage, native checks, and diagnostic limits.

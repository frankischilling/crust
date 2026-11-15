# Crust v0.5.9

Ctrl+F search now reserves the full height of its card, keeping chat messages
below the border and help text. Short search windows scroll so every field and
regex diagnostic remains reachable. Reopening search selects the existing query
for replacement.

## Search and keyboard controls

- Global search reuses measured row heights. Steady frames lay out the visible
  results and any focused or selected scroll target. Wrapping, font changes,
  edited messages, and resized windows invalidate the affected measurements.
- Load older buttons keep their identity as results move. Keyboard navigation
  scrolls the selected result into view, and Ctrl+F returns to the current search.
- Emote picker shortcuts work from its buttons and emote cells as well as its
  search field. Escape closes the picker, Ctrl+F selects its query, and Enter
  activates the focused control. Emote cells have a visible focus outline.
- ASCII search comparisons avoid allocating lowercase copies. Unicode searches
  retain the existing lowercase matching behavior.

## Subscriber messages

- Subscription and resubscription notices show the subscriber's message with
  native emotes, links, and wrapping. Source IDs, sender metadata, and timestamps
  survive the normal chat and local-history pipeline.
- Search, copy actions, user-card text, and text exports include the notice and
  message once, including subscription rows saved by older versions.
- Subscriber messages follow ignored-phrase and moderation rules. Gift-recipient
  mentions use the recipient's login even when their display name is localized.

## Live emote updates

- 7TV subscriptions share up to eight bounded connections. Each topic has one
  owner, and every connection respects the limit announced by the service.
- Shared emote sets avoid duplicate subscriptions. Closing rooms and switching
  accounts remove their topics, and stale connection events cannot replace a
  newer state.
- Scopes awaiting a connection or exceeding the connection budget receive
  periodic metadata and cosmetic refreshes. Background requests remain bounded.
- Failed startup cosmetic loads retry with a capped delay. A successful load
  stops retrying, and late responses preserve newer catalog data and live edits.
- Reconnected subscriptions receive a fresh cosmetic baseline after acknowledgment,
  including channel topics split across connections.

The [search and live-update audit](https://github.com/frankischilling/crust/blob/v0.5.9/docs/AUDIT_SEARCH_AND_LIVE_UPDATES.md) records
validation, measurements, and remaining work.

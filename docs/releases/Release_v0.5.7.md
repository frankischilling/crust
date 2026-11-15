# Crust v0.5.7

Mentions now renders emotes, overlays, emoji, links, and action text through the
same message-body renderer as channel chat. This release also fixes saved-layout
restoration, stale spelling suggestions, phrase replacement, and controls that
could drift or become unreachable.

## Chat rendering and controls

- Mentions displays ordinary and layered emotes in live and restored messages.
  Deleted messages keep readable secondary text with a strikethrough. Animation
  pause and hidden link previews follow the application settings.
- Clicking a message or emote opens its source channel. Links open their URLs,
  and Copy message preserves the original emote codes and whitespace.
- Changes to parsed spans or image dimensions invalidate the affected Mentions
  row heights. Offscreen measurement does not request or decode emote images.
- Sidebar paint, click targets, and drag positions use the same row geometry.
  Long labels and unread counts have separate bounds, close buttons use the
  shared vector icon, and channel controls expose names and selection state.
- Clicking a channel name or unread badge selects the channel even when message
  text selection is enabled.
- Live indicators apply only to the matching Twitch channel.
- Toolbar controls use measured widths at the current font size. Long account
  names truncate within their button, and collapsed actions remain available in
  the overflow menu.
- Long failed drafts, reply context, and error details scroll within the recovery
  dialog while its recovery controls remain reachable.

## Saved layouts

- Restoration normalizes legacy channel IDs and removes channels that cannot
  join with the current configuration. Valid channels that are still connecting
  retain their saved place.
- Split widths are applied after all panes exist. Filtering keeps the intended
  focused pane, including repeated channels with separate drafts.
- Live and Mentions selections and split panes survive a save/restore round trip.
- Channel redirects update pending saved references, so an old channel name
  cannot leave restoration waiting indefinitely.
- Manual navigation, layout changes, or typing cancel an older pending restore.
  This includes the first text or paste event and IME composition. Automatic
  restoration also preserves focus in other controls.

## Spelling suggestions

- Suggestion generation searches plausible dictionary entries with a bounded
  edit check. Normal typo corrections, adjacent transpositions, custom words,
  frequency ranking, and phonetic fallback remain available.
- Five interleaved optimized-process samples measured a 16-character unknown
  word at 83.967 ms before the change and 1.128 ms afterward. These measurements
  cover suggestion generation, not application startup.
- Cached choices belong to one exact draft and channel. Edits and channel
  changes invalidate stale choices, and replacement checks its UTF-8 range again.

## Phrase rules

- Case-insensitive literal replacement preserves surrounding Unicode text.
  Literal replacement values such as `$1` stay literal; regex rules keep capture
  expansion.
- Replacing unrelated text preserves native emote identities and adjusts their
  character offsets. Only emote occurrences overlapped by an edit are removed.
- Live chat, remote history, and incoming whispers apply phrase rules before
  display or storage. Blocked whispers do not trigger sender loading or
  notifications. Stored local history is not transformed again on replay.
- Mention-only rules keep their flag when ordinary account-name detection runs.

The [audit and measurements](../AUDIT_CHAT_POLISH.md) describe the checks and their
limits. The investigation into occasional long UI updates remains open in
[issue #17](https://github.com/frankischilling/crust/issues/17).

# Crust v0.7.2

This release fixes sent-message matching, draft recovery, unread badges, and link
previews. Image validation now runs on at most two workers, and downloaded images
keep their download slots until queued completions are consumed or dropped.

## Chat and replies

- Local sent messages use the same phrase rules, nickname aliases, and mention
  detection as incoming messages. Replacements adjust native-emote positions
  while preserving untouched emotes.
- Server confirmations match the original message body even if display rules
  change while a send is pending. Different messages that produce identical
  display text remain separate, and confirmed messages keep one saved history row.
- Reply context follows the parent ID sent to the server. Replies with only an
  ID show the exact retained parent when available, with a neutral fallback when
  the parent is unavailable.

## Interface and recovery

- Choosing Restore brings the original composer into view, including inactive
  channels, closed panes, and detached windows. Newer drafts and closed-view
  history are preserved. Automatic recovery does not switch views.
- Focusing a split clears its unread and mention counts without resetting its
  search, draft, or paused scroll position. The Mentions feed follows the same rule.
- The reply banner uses the shared Close icon and avoids blank names or dangling
  punctuation when the parent message is unavailable.
- Escape closes Settings when it owns keyboard focus. Search, shortcut capture,
  the expression editor, and menus handle Escape first, so one press
  dismisses one level at a time.
- Resume scrolling is exposed as a named button and shows keyboard focus. It can
  resume live chat through a pointer click or keyboard activation.

## Previews and images

- Preview titles and descriptions appear without waiting for a thumbnail. Visible
  thumbnails use the existing image queue, avoiding a second download path.
- URL handling checks the actual hostname and path. Tweet proxy rewrites preserve
  the rest of the URL, and Twitch clips receive the correct site label.
- Metadata parsing preserves Unicode offsets, reads complete attributes and long
  values, and resolves relative image URLs against the page's final address.
  The HTML reader keeps the first 64 KiB; larger oEmbed responses are rejected.
- Image validation checks native and RGBA output sizes before decoding, with a
  128 MiB output limit per image. Cancelled validation keeps its worker slot until
  the work stops. Validation covers a static image or the first animation frame.
- Queued image completions retain their download slots through forwarding and
  cloning, so waiting payloads share the existing 20-download limit.

The [audit](https://github.com/frankischilling/crust/blob/main/docs/AUDIT_CHAT_RECOVERY_AND_PREVIEWS.md)
records the defects, regression coverage, and live measurements. Intermittent UI
stalls remain tracked in [#17](https://github.com/frankischilling/crust/issues/17).

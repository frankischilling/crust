# Crust v0.7.1

This release fixes sent-message confirmations, compact channel banners, and image
loading. Static PNG and JPEG decoding now uses the background queue, and animated
emotes handle missing or very short frame delays.

## Chat state

- Confirming a sent message imports server flags, reward metadata, and Shared Chat
  information while retaining its local identity and accumulated moderation state.
- An identical Shared Chat mirror cannot consume a pending send from another
  source channel. Saved history follows the same matching rule.
- Confirmations that first qualify for Mentions add the missing row without
  another unread count or notification. Repeated confirmations remain deduplicated.
- Moderator and lead-moderator badges enable the same role state used by the send
  rate limiter, including when the separate moderator tag is missing or stale.

## Interface

- Room-mode labels wrap when space is limited. Hype Train metadata and Shared Chat
  participant labels stay inside narrow panes, with full details available on hover.
- Raid text wraps around a shared Close icon with a named accessibility action.
- Closing a channel that is still loading removes it from the startup status.

## Images

- Static image requests return while decoding proceeds on the existing bounded
  worker queue. Cache limits, cancellation, and custom image loaders remain supported.
- Static name-paint sampling shares decoded pixels with ordinary image rendering.
- Queued and running image jobs share a 128 MiB encoded-data limit. Cancelled work
  stays charged until it releases its source bytes.
- GIF and WebP frames with delays of at most 10 ms use 100 ms. Other delays below
  20 ms use 20 ms, preventing stalled or excessively fast playback.

The [audit](../AUDIT_CHAT_STATE_AND_IMAGES.md) records the defects, regression
coverage, and measurements. The intermittent UI stalls remain tracked in
[#17](https://github.com/frankischilling/crust/issues/17).

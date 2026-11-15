# Crust v0.5.6

This release fixes history replay after moderation, adds layered emotes, and
keeps completion menus usable in short windows. It also reduces the work needed
to load Mentions history and normal-size emote images.

## Chat and history

- Deletions, timeouts, and channel clears update both the channel and Mentions.
  Saved history and late history responses retain the deletion state.
- User and channel clears use source timestamps when available, so a delayed
  event does not hide newer messages. Events without timestamps affect only
  messages already known when they arrive.
- Remote history preserves deletion markers and applies moderation records.
- Confirmed sent messages update their existing Mentions and saved-history rows.
  Deletion and highlight flags survive confirmation without duplicate unreads
  or a leftover unconfirmed row returning after restart.
- The background writer reconciles tracked confirmations before the UI consumes
  them, covering a close-and-reopen race. Local echoes use the successful send
  time to keep queued messages in the correct order relative to channel clears.
- Mentions uses an indexed SQLite query. Live messages, highlight changes, and
  whispers use an ordered background writer; history reads wait for earlier
  queued writes.
- Highlights and sent-message confirmations keep a separate bounded queue, so
  a full action queue cannot discard them. Closing the window preserves its
  pending history updates for shutdown.
- Mentions match whole names, including Unicode boundaries. Nickname aliases
  follow their configured channel scope in live chat and remote history.
- Whisper mention counts reject longer usernames. Historical rows receive
  custom-highlight flags for search without replaying live notifications, and
  restored deletion markers refresh open search results.
- Invalid or overlapping native emote ranges cannot duplicate message text.

## Emotes

- 7TV overlays and recognized BTTV global overlays layer over the preceding emote in chat and whispers.
  Successive overlays share one layout item. An overlay without a preceding
  emote remains visible as an ordinary emote. Copied and saved text stays intact.
- Live changes to an emote's overlay flag update existing messages and their
  layout, including the Mentions view.
- Failed channel providers retry after a 30-second cooldown. Healthy providers
  keep their catalogs, and valid empty results do not cause repeated requests.
  Routine room-state updates cannot bypass the cooldown.
- FFZ catalogs retain animated assets and load only the declared global sets.
- Normal 7TV rendering prefers smaller supported assets. HD previews keep the
  larger images, and provider requests share an HTTP connection pool.
- Images follow their actual animation deadlines. Static WebP images no longer
  force repeated redraws, and painting no longer scans payloads for animation markers.
- Pausing animations also covers picker and completion images, whispers, and
  hover previews, including image URLs without a file extension.
- Plugin emote catalogs and message spans include a `zero_width` flag.

## Controls

- Completion menus scroll within the window and adapt to enlarged fonts. They
  stay clear of the composer, return focus after pointer selection, and keep
  keyboard selections visible.
- Escape dismisses completions while keeping the draft and editor focus. A new
  filter selects the first match; overlapping controls do not activate the menu.
- Command descriptions and provider labels have readable text colors, including
  selected rows. The composer has a vector emote-picker icon.
- Search navigation follows its input or result focus. Clear, Close, and other
  focused controls retain their keyboard behavior.
- Mentions and Live hide the channel stream header, so virtual views do not show
  internal channel names or a stream-status request.
- Live and Mentions have named accessibility controls with an active state.
  Each Live row also exposes an action to open its channel.
- Plugin action calls report whether they were queued. Full queues return a
  reason without blocking or panicking inside an asynchronous callback, and
  plugins can retain their inputs for an explicit retry.
- Notification sounds open the audio device and load custom files on a bounded
  worker. Muting or changing settings cancels waiting sounds. Desktop popups
  no longer add system sounds that bypass the configured volume and mute settings.
- Slow-update diagnostics identify the event being handled without recording
  message contents.

The [audit and measurements](../AUDIT_CHAT_RELIABILITY.md) describe the test
workloads and verification. The earlier investigation into occasional long UI
updates remains open in [issue #17](https://github.com/frankischilling/crust/issues/17).

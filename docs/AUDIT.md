# Chat behavior and performance audit

Audit date: 2026-11-14. Starting revision: `2555d6a` (v0.5.1).

The changes are tracked in [pull request #6](https://github.com/frankischilling/crust/pull/6).

This pass covers the Rust workspace, chat transport and moderation behavior, message
history, image loading, egui controls, plugin callbacks, and project documentation.
The fixes below address reproduced failures and code paths with a clear failure
case. This is a record of the work and its validation limits, not a guarantee that
the application has no remaining bugs.

## Build and service integration

Internal crate versions had drifted from the workspace release. One dependency
still requested v0.4.9, so Cargo could not resolve the workspace. Internal paths
and versions now come from the workspace dependency table.

Several live runtime callers referenced missing APIs. The Twitch crate now
provides the channel-points and public Hype Train GraphQL reads, active poll and
prediction reads, current Hype Train status, raid source login, and Hype Train
EventSub lifecycle data.

Helix requests normalize an optional `oauth:` token prefix and have request
deadlines. Account changes no longer wait for a pending HTTP read lock, and a
response from a replaced client is rejected. Reconnect refresh jobs cancel when
authentication changes, including helpers that use a captured token directly.

Reconnect snapshots distinguish an unchanged object from a changed phase or vote
total. A successful response reporting no active Hype Train clears an old banner;
a failed request preserves it. The clear carries the request's start time so it
cannot erase a newer live update.

The web GraphQL queries have ten-second deadlines and a 256 KiB response limit.
They distinguish a missing account, a missing channel, an unavailable points
program, and an unexpected response shape. A real zero balance is preserved.
Missing Hype Train fields produce an error rather than an invented end event.
Tokens remain in the Authorization header and are redacted from query errors.

## Connection recovery

EventSub subscription HTTP runs in a separate serial worker, so slow requests do
not block socket commands or keepalive handling. Removing a channel immediately
filters its events and deletes its remote subscriptions. A creation request that
finishes after removal is cleaned up. Each HTTP request has a ten-second deadline.

A server-requested socket transfer keeps the old socket open until the replacement
sends its welcome. The subscription worker survives the transfer and finishes any
pending creation or deletion before recovering the new session's subscriptions.
Recovery only accepts IDs owned by that WebSocket session. Logout and account
changes cancel the old worker.

Initial connections and welcome messages have ten-second deadlines. After welcome,
EventSub traffic refreshes the advertised inactivity deadline; WebSocket Ping/Pong
and malformed JSON do not. A silent socket reconnects, and a replacement socket uses
its own advertised timeout. Reconnect backoff processes channel and account changes
without waiting for the delay to finish. An account change wakes the connection
attempt immediately, while channel updates keep the original retry deadline.

IRC registration now has its own ten-second deadline after the socket connects.
An idle registration sends a probe before timing out. Socket writes also have
deadlines, and reconnect waits continue to process account and channel commands.
A successful registration resets the retry delay. Chat submitted while
disconnected gets an error instead of joining a queue that cannot drain.

Token validation has a ten-second deadline and a request generation. Logout,
account removal, and a newer login invalidate older results. Saved-account
validation checks the returned login, and session events carry a generation so
late role or authentication updates cannot restore a previous account.
Refreshing the same identity updates its services without reconnecting IRC.

An explicit account change detaches the old identity before validation starts.
Chat, whispers, and moderation actions require an established account; rejected
chat keeps its text and reply for recovery. If validation fails, the client stays
anonymous and retains the saved accounts. It cannot silently send a delayed
action as the old account. Recovery restores an empty matching composer or keeps
the message available separately when a newer draft is already present.

## Chat and moderation

- IRC and EventSub copies of the same timeout, ban, or clear merge into one row.
  The richer EventSub text retains the moderator and reason. Matching is bounded
  to the latest 20 rows and five seconds; repeated actions from one source remain
  separate.
- Server chat clears mark user chat deleted and preserve moderation notices.
  The local clear command still empties the buffer.
- Timeouts and bans also delete matching Bits and suspicious-user message rows.
- A channel-only `CLEARCHAT` no longer treats the channel name as a banned user.
- Malformed messages without bodies no longer become messages containing a
  channel name. IRC tag escapes are decoded once.
- Live and replayed messages share parsing for timestamps, Bits, replies, shared
  chat source metadata, and source badges.
- Successful IRC registration emits one connected event. Server reconnect
  requests and rejected credentials are handled promptly. Initial IRC connection
  attempts have a ten-second deadline.
- Outgoing chat validates message framing, channel targets, and reply IDs.
  Twitch JOIN and PART commands also validate their channel targets.
  Generic IRC rejects embedded CR, LF, and NUL before writing bytes.

Twitch sends now follow rolling limits: 20 messages per 30 seconds per user,
with at least one second between messages in an ordinary channel. Broadcasters,
moderators, and VIPs can continue to 100 messages per 30 seconds; their messages
still count against the ordinary limit. JOIN attempts are limited to 20 per ten
seconds.

The connection queues at most 256 chat messages, 256 KiB of encoded chat lines,
and 512 JOINs. Each channel keeps its message order, while eligible channels can
proceed. Repeated JOIN requests are ignored while a channel is queued or joined.
PING replies, account changes, and disconnects remain available during rate waits.
Leaving a channel cancels its unsent output; reconnecting discards the connection's
pending output. Sent rate history survives reconnects and account changes, so a
newly selected account can inherit the remaining wait.

## History and filtering

History merges remove repeated server IDs both within an incoming batch and
against the current buffer. Messages with no server ID remain distinct, and
existing live messages take precedence over repeated history.

Stored history deduplicates server IDs per channel. A small count table and
transactional triggers maintain the exact 50,000-row limit without scanning the
retained history on each append. Migration, multiple connections, rollback,
out-of-order timestamps, and equal timestamps have regression coverage.

Historical mentions do not increase unread counts. Shared-chat mirrors marked
`suppress_notification` remain excluded from Mentions after restart, including
when a small query limit is used.

The filter context now reports shared messages and whispers correctly. Hide
filters apply without an active search or ignore list, and they also apply to
paused chat. Dim-only rules retain the existing fast path.

Nickname changes retain the sender's original display name. Editing or removing
an alias updates live chat, loaded history, mentions, and user cards. The added
field is optional when reading older records. An older record that already
overwrote its original name cannot recover information that was never stored.

Global search notices messages arriving after a channel reaches its retention
limit, as well as edits to existing messages and changes to aliases. It selects
the newest 5,000 matches before cloning results. Channel groups and keyboard
selection use the same order; refreshing preserves the selected message when it
still matches. A result in another visible split focuses that split and preserves
both drafts.

## Interface and input

Only the focused composer consumes completion, history, and paste shortcuts.
History recall starts at the newest entry and restores the draft when moving
forward again. The Send button resets completion/history state and returns focus
to the composer.

Image paste events are deduplicated across key press, text, and delayed key
release events. A dropped image targets one pane. Emote-picker requests remain
retryable when the UI command queue is full.

Reselecting a channel preserves its draft. Selecting a channel already visible
in another split focuses that pane instead of replacing the current pane.

Redirecting a channel onto an open destination merges their history and preserves
the destination's metadata. Both split-pane drafts and the focused pane survive
the redirect.

Header and sidebar labels leave room for their controls. The split Close button
stays at the right edge at the tested widths of 150, 210, and 360 pixels. Composer
tray height follows its controls and font scale.

Search previews wrap at the current width, and each group leaves room for its
Load older button. Settings keeps its title and Close control inside the viewport
after resizing or zooming. Its navigation and content scroll separately; wide
layouts give most of the width to the settings fields. Oversized controls remain
accessible through the content area's horizontal scrollbar.

Wheel input pauses chat only when the pointer reaches the chat panel's own UI
layer. Scrolling Settings or another overlapping window no longer pauses the
chat behind it.

Historical messages, suppressed mirrors, and background-channel activity do not
generate active-channel event toasts. Moderation rows keep their detailed text
when rendered.

Shortcuts match their modifiers exactly, so reverse navigation and shifted
moderation shortcuts are no longer consumed by their unshifted bindings.
Composer Cut and Copy use the selected Unicode text. A rejected send keeps the
draft and reply, and settings rows have visible remove buttons with tooltips.
Completion replaces the token at the caret, preserves the following text, and
supports repeated Tab and Shift+Tab cycling without losing focus.

UI zoom multiplies the display's native density, including density changes when
moving the window between monitors. It no longer replaces the operating system's
scale with an absolute pixel density.

The Windows Yu Gothic fallback has a baseline adjustment for mixed Japanese and
Latin/Greek text. Its larger line gap previously placed glyphs above their row
and into the preceding line. A geometry regression covers two font families,
four sizes, and three display densities when the optional font is installed on
Windows; it skips when that font is unavailable. Native screenshots confirm the
spacing on the tested system.

The UI never waits for capacity in the runtime command channel. A shared FIFO
keeps account changes, chat, and widget actions in submission order, with at most
128 deferred commands and 32 MiB of deferred upload data. Each frame flushes at
most 64 commands after reading runtime events. A full queue reports the problem
without discarding the current composer draft.

Message measurements are invalidated when width, display scale, font sizes,
spacing, or relevant controls change. Entries for discarded messages are pruned,
and prefix sums follow the visible message sequence. Jumping to an older search
result expands the history window and pauses live following so the target stays
visible.

## Images and rendering cost

Image responses must succeed over HTTP and contain decodable image data before
they enter the cache. Corrupt or expired disk entries are refetched. Disk age is
preserved when bytes enter memory, so reading a nearly expired disk entry does
not grant it another full day.

Cached image requests for the same URL share disk/network work. Cancelling the
request owner releases its waiters. The application retries transient failures
with bounded backoff, queues one request during cooldown, and limits concurrent
downloads across loaders to 20. Retry sleeps do not occupy download slots.

Active downloads and completed images waiting for UI event admission share those
20 slots, including avatar/profile fetches. Successfully delivered images use
the event queue's separate capacity. Cancelling delivery or closing the receiver
releases the slot and leaves the URL retryable.

The failure state is published before the empty completion event, allowing a UI
caller to request another attempt immediately without losing it to an obsolete
in-flight marker.

Image downloads stop at 32 MiB of encoded data, including responses without a
Content-Length header. Oversized and failed responses do not enter the cache.

The UI shares image storage across chat, menus, avatars, and thumbnails. Its
retained caches have separate limits:

| Cache | Byte limit | Entry limit |
| --- | ---: | ---: |
| Encoded image data | 128 MiB | 4,096 images |
| Frozen first frames and paint samples | 64 MiB | 1,024 images |
| Decoded images and animation frames | 128 MiB | 4,096 source URLs |
| Uploaded textures | 128 MiB | 4,096 frame/texture-option pairs |

Assets used in the current paint pass stay available until that pass finishes.
Evicting an animation releases its uploaded frames as well as its source data.
If only decoded frames are evicted, reusing a surviving texture restores animation
timings so the image continues to advance.
Visible cache misses can request an image again even after a previous download
succeeded; background prefetches still deduplicate successful requests. Retries
remain eligible when the command queue is full, and automatic image requests
wait behind queued user actions without producing repeated action-failure notices.

Decoded canvas size and GIF/WebP frame counts are checked before the UI invokes
the decoder. These cache limits describe retained image data, not total process
memory. Runtime image-request bookkeeping is also capped at 16,384 URLs.

Shared badges use borrowed references instead of building a cloned vector and
hash set for every visible message. Existing chatter names avoid repeated
allocation. Spellcheck word scanning uses one Unicode pass, and unfocused
composers avoid completion scans.

## External commands and uploads

External-player templates are split before channel names, paths, and URLs are
substituted. Windows drive paths, UNC paths, quoted directories, and empty quoted
arguments retain their intended argument boundaries. Placeholder-like text in a
replacement value is literal. Child stdout and stderr drain concurrently while
retaining only the last 64 KiB of each stream, so a verbose process does not keep
growing the client's memory or block on a full pipe.

Upload response templates expand in one pass over the original template.
Response values containing placeholders cannot recurse indefinitely, and JSON
is parsed once for the image and deletion URLs.

## Plugins and test reliability

Exported numeric event constants and callback registration previously used
different orderings. Events after the inserted settings entries could register
the wrong callback. One ordered table now drives both directions while preserving
the existing numeric parser IDs.

Plugin tests use a separate temporary directory for each test and clean it up
during unwinding. They no longer share fixtures or change process-wide home
directory variables. This exposed callback failures that the shared fixtures had
obscured.

The UI's 101 implicit floating-point fallback warnings are fixed with explicit
`f32` stroke widths. The edits follow compiler suggestions and do not change
control dimensions.

## Measured hot paths

These are isolated measurements on the Windows development toolchain, using the
repository's optimized development/test profile. They do not measure total
application speed, frame rate, network latency, or disk durability cost.

| Workload | Previous path | Revised path | Scope |
| --- | ---: | ---: | --- |
| Event-ID deduplication: 20,000 operations, 10,000 retained IDs | 530.4591 ms | 3.5281 ms | About 150× faster for the lookup/eviction workload |
| History retention: 500 appends with 50,000 retained rows | 473.8627 ms | 16.6623 ms | Median of five rounds; about 28.4× faster with SQLite 3.45.0 in memory |

The retention comparison includes message serialization, locking, insertion,
retention, and transaction commit in both paths. Setup and migration are outside
the timed region. Runs alternate order and assert the same retained row count
and timestamp range.

A ten-second parser/reducer replay at 1,000 messages per second processed all
9,983 produced messages, with no parse errors or final backlog. Peak measured
frame work in that fixture was 0.737 ms.

Reproduce the isolated comparisons with:

```bash
cargo test -p crust-twitch --lib perf_event_id_deduplication --locked -- --ignored --nocapture
cargo test -p crust-storage --lib perf_log_retention --locked -- --ignored --nocapture
```

## Validation scope

Regression tests use temporary databases, loopback HTTP/WebSocket servers,
synthetic chat events, and headless egui interaction/layout checks. They cover
malformed input, repeated events, cancellation, logout during requests, image
failures, history boundaries, and narrow controls.

The final Windows workspace run passed 809 tests, with nine benchmark/soak tests
ignored by the normal run. The isolated measurements above were run separately.
Both application binaries built successfully, and Clippy passed with correctness
errors enabled. Nonblocking lints remain for large enum/error layouts, plus two
unused Linux updater helpers in the Windows test build.

Documentation checks covered 157 relative links across 46 first-party Markdown
files. All file, directory, heading, and wiki-page targets passed.

The main verification commands are:

```bash
cargo test --workspace --all-targets --locked
cargo build -p crust -p crust-webview-host --locked
cargo clippy --workspace --all-targets --locked -- -A clippy::style -A clippy::complexity -D clippy::correctness
```

Live GraphQL checks used anonymous, read-only requests. Both query shapes were
accepted by the service. A signed-in points balance and an active live Hype Train
were not exercised. These reads use the web client's schema, which can change
independently of Helix.

A native Windows fixture runs the production interface with synthetic channels,
messages, static images, and animations. Screenshots were inspected at 600 × 800,
1,125 × 875, and 1,200 × 950 pixels, using the display's 1.25 native density and
UI zoom factors of 1.0 and 1.5. The checks covered toolbar and channel controls,
chat and search wrapping, mixed Japanese text, emote rendering, the unsent-message
panel, and Settings bounds and lower controls after scrolling. The fixture uses
local data and does not sign in or send chat.

Real-account sign-in, switching, and moderation, Linux packaging, and release
performance profiling remain unverified. Automated account-transition tests
exercise the state changes and delayed callbacks without a live account.

## Documentation

The README now gives an overview and links to dedicated build and feature
guides. Documentation and wiki prose was revised for direct wording, accurate
behavior, and consistent navigation. Existing API names, examples, release
dates, and licensing information are preserved.

Service behavior was checked against the official
[Twitch chat limits](https://dev.twitch.tv/docs/chat),
[IRC concepts](https://dev.twitch.tv/docs/chat/irc),
[Helix API](https://dev.twitch.tv/docs/api/reference/), and
[EventSub reference](https://dev.twitch.tv/docs/eventsub/eventsub-reference/).
Connection recovery also follows the
[EventSub WebSocket lifecycle](https://dev.twitch.tv/docs/eventsub/handling-websocket-events/).

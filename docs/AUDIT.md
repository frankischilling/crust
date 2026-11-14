# Chat behavior and performance audit

Audit date: 2026-11-14. Starting revision: `2555d6a` (v0.5.1).

These changes are on `fix/chat-audit` and await review and merge.

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

Historical messages, suppressed mirrors, and background-channel activity do not
generate active-channel event toasts. Moderation rows keep their detailed text
when rendered.

## Images and rendering cost

Image responses must succeed over HTTP and contain decodable image data before
they enter the cache. Corrupt or expired disk entries are refetched. Disk age is
preserved when bytes enter memory, so reading a nearly expired disk entry does
not grant it another full day.

Cached image requests for the same URL share disk/network work. Cancelling the
request owner releases its waiters. The application retries transient failures
with bounded backoff, queues one request during cooldown, and limits concurrent
downloads across loaders to 20. Retry sleeps do not occupy download slots.

The failure state is published before the empty completion event, allowing a UI
caller to request another attempt immediately without losing it to an obsolete
in-flight marker.

Shared badges use borrowed references instead of building a cloned vector and
hash set for every visible message. Existing chatter names avoid repeated
allocation. Spellcheck word scanning uses one Unicode pass, and unfocused
composers avoid completion scans.

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

The final Windows workspace run passed 685 tests, with nine benchmark/soak tests
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

Native-window visual inspection, real-account moderation, Linux packaging, and
release-performance profiling are outside the completed automated checks.
Headless layout tests verify control geometry and input routing; they do not
replace checking the running application at different DPI settings.

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

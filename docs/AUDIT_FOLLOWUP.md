# Chat delivery, emotes, and history follow-up

Date: 2026-11-14. Starting revision: `73f3a98`.

This pass continues the [earlier audit](AUDIT.md). It examines send completion,
saved-history boundaries, emote scope, keyboard focus, and repeated work during
rendering. Work is tracked in [issue #8](https://github.com/frankischilling/crust/issues/8).

## Send completion

The runtime previously published a local Twitch echo before the session accepted
or wrote the message. An offline connection or a full send queue could therefore
leave a message in chat that had never been sent. Each queued send now carries a
completion receipt. The runtime publishes and saves the captured sender, text,
and reply only after the write succeeds. Failures return the original draft to
the existing recovery UI.

Generic IRC follows the same completion path. Its echoes use the server-confirmed
nickname, including collision fallbacks, and `/me` is encoded as an action.
Rejected nickname changes cannot relabel subsequent local messages.

Completion publication preserves each channel's order while allowing other
channels to proceed. It has a 512-item bound, checked before transmission. Action
messages keep their action body, and a failed whisper does not enter saved
history. A successful socket write is not a server acknowledgement.

## Local history

Timestamp-only paging could skip messages sharing a millisecond. Paging now uses
the indexed `(timestamp, row ID)` pair and reads one extra row to detect whether
more history exists. Corrupt rows advance the cursor without blocking later
pages. Empty pages carry completion metadata so the UI can stop its loading
indicator and decide whether another page remains.

Local replays use stable IDs derived from their SQLite rows. Matching live rows
retain their existing UI IDs, while distinct saved rows remain separate even
when their original IDs, timestamps, and text match. This prevents repeated loads
or buffer eviction from duplicating messages without server IDs.

An explicit older-history load can expand a channel's retained buffer from 1,500
to 50,000 messages. Ordinary live buffers retain the existing 1,500-row limit.
Search exposes older-history controls even when the current buffer has no hits
and disables them after exhaustion. Newly applied live highlights update the
saved row so Mentions can restore them after restart.

## Emote scope and refresh

The old shared code map allowed one channel's emotes to appear in another and
treated a broadcaster's set as a sender's personal set. The registry now keeps
global, channel, and sender-owned definitions separately. In channel chat, runtime tokenization,
autocomplete, and the picker share the same availability and priority rules.
Native Twitch emote positions remain authoritative for tagged messages.

Global BTTV, FFZ, and 7TV sets load alongside channel sets. Personal sets come from
the provider's flagged personal-set metadata. Sender profile and personal-set
discovery share a query, with bounded requests, response sizes, cached users,
emote counts, and retry delays. Generation checks reject obsolete channel and
account jobs.

Incoming whisper events do not yet carry the sender's user ID, so those whispers
use global emotes. A self-whisper can use the authenticated sender's personal set.

Catalog invalidation checks content, including normal and larger image URLs.
Removing an emote restores text in existing messages. Retokenization processes
at most 400 retained messages per update, including saved history, Mentions, and
whispers. New catalog snapshots preserve scan progress and coalesce a follow-up
pass. Changed rows invalidate their measured layout heights.

## Input and navigation

The picker consumes keys only while it owns focus. Forward and reverse Ctrl+Tab
keep focus in its search field; ordinary Tab can leave it. Opening the picker
focuses search, Escape restores composer focus, and picking an emote replaces the
Unicode selection or inserts at the caret. Favorites menus use stable cell IDs.

Split composers have stable pane identities. Keyboard navigation and clicks in
child controls select the correct pane, and adding, moving, or removing panes
keeps the active draft attached to its composer. Completion cycles are cleared
when the channel or catalog changes. Channel dragging stores the channel ID so
changes to the filtered list do not redirect a drag to another channel.

## Repeated work and memory

Highlight regexes compile when rules change. Scoped keywords retain their
case-insensitive behavior, while explicit regex modes and inline flags remain
available. Search fixes cover literal punctuation, unknown tags, subscription
tiers, and comma-separated message flags.

Paused chat caches the mapping from snapshot IDs to retained rows. Unchanged
frames reuse it; replacements, reordering, and eviction rebuild it. Hide filters
operate on that mapping without permanently deleting snapshot IDs.

The runtime's encoded-image cache retains at most 128 MiB across 256 entries.
Expired entries and replacement entries update byte accounting correctly.
Encoded disk reads have a 32 MiB limit. These bounds supplement the UI image and
texture caches described in the earlier audit; they are not a process-memory
limit.

### Isolated measurements

These measurements use the optimized Windows development/test profile. Each
number is the median of five rounds with alternating execution order. They
measure the named work, not application frame rate or total memory use.

| Workload | Previous path | Revised path |
| --- | ---: | ---: |
| 10,000 scoped regex highlight checks | 1,246.74 ms | 0.2567 ms |
| 5,000 paused-frame index checks over 1,500 rows | 185.3634 ms | 7.8549 ms |

The highlight comparison alternates matching and nonmatching text and checks
that both paths return the same result. The paused-chat comparison uses an
unchanged buffer; separate regressions cover eviction, interior changes, and
hide-filter changes.

```bash
cargo test -p crust-core --lib perf_scoped_highlight_matching --locked -- --ignored --nocapture
cargo test -p crust-ui --lib perf_paused_message_indices --locked -- --ignored --nocapture
```

## Build and verification

Both packaging scripts use the lockfile. Windows checks native command exit
codes before copying binaries. Debian uses linked-library metadata for ABI
requirements and explicitly includes the X11 keyboard library loaded at runtime.
Checksums refer to filenames rather than build-machine paths.

The Linux workflow runs release-profile workspace tests and correctness lints,
builds a Debian package, and checks desktop startup under a virtual X11 display.
Windows verification uses the workspace tests and an offline native UI fixture.
The fixture runs the production widgets with synthetic messages and images.

The final Windows workspace run passed 905 tests, with 11 benchmark and soak
tests ignored by the normal run. The two isolated comparisons above ran
separately. Clippy passed with correctness errors enabled; existing large-enum,
large-error, and Windows-unused Linux-helper warnings remain. Native screenshots were inspected for chat, the picker, and detached
search at narrow and enlarged layouts, including 600 × 800 and 1,200 × 950 pixels
with 1.25 native display density and UI zoom factors of 1.0 and 1.5.

The packaging parsers were checked with fresh Cargo artifacts, custom paths,
missing artifacts, incompatible binary architectures, and failed builds. The
Debian shell script also passed syntax and parser checks under WSL.

Reproduce the normal checks with:

```bash
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -A clippy::style -A clippy::complexity -D clippy::correctness
```

Live account sign-in, sending, and moderation require account-level testing.
Loopback tests exercise socket writes, rejection, cancellation, and reconnect
paths without sending messages to a real channel. Provider APIs can change
independently of this client.

## Remaining work

- [Issue #9](https://github.com/frankischilling/crust/issues/9) tracks native
  subscription emote inventory, incoming-whisper ownership, name paints, and live
  provider set updates.
- [Issue #10](https://github.com/frankischilling/crust/issues/10) tracks cancellation
  during the initial EventSub connection attempt and generic IRC private-message
  routing.
- [Issue #11](https://github.com/frankischilling/crust/issues/11) tracks redundant
  badge, history, and pinned-message loads on room-mode updates.

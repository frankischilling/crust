# Search, subscriber messages, and live updates

This pass addresses the Ctrl+F overlap, keyboard focus in search and the emote
picker, missing subscriber messages, and live-update coverage for large channel
sets. The tracked fixes are [#40](https://github.com/frankischilling/crust/issues/40),
[#41](https://github.com/frankischilling/crust/issues/41),
[#38](https://github.com/frankischilling/crust/issues/38), and
[#42](https://github.com/frankischilling/crust/issues/42).

## Search layout and controls

The inline search card returned its content height to the parent layout. That
omitted the frame's border and padding. At a 680-point viewport, the regression
test measured 122 points reserved for a card occupying 144 points. The same overlap
was visible in the signed-in Windows app: the first chat line crossed the help
text at the bottom of the search card.

The card now returns the frame's full height. Detached search windows use a
vertical scroll area, so resizing or a long regex diagnostic cannot make the
lower fields unreachable. Reopening search selects the existing keyword. A
scrolled window brings the whole field, including its border and padding, back
into view. A focused global-search window keeps Ctrl+F in that window.

The emote picker now owns its navigation shortcuts while any picker control has
focus. Escape closes it after Tab navigation, Ctrl+F selects its filter, and
Enter preserves the normal action of the focused button or emote. Emote cells
show a focus outline and expose their names to accessibility tools.

## Search rendering cost

Global search retains up to 5,000 results. It caches variable row heights and
skips layout for cached rows outside the viewport. The selected scroll target
and any focused row still render. Group headers retain stable interaction IDs
so Load older keeps focus as surrounding rows move.

Measurements are invalidated by width, scaling, style, fonts, visible preview
text, sender names, and timestamps. Removed results are pruned. The first pass
still measures unknown rows; steady frames scan the bounded cache and lay out
only visible or focused rows. This optimization does not make the initial
search or filtering independent of the retained message count.

A controlled release benchmark with 5,000 results measured a warm-frame median
of 1.409 ms with cached rows, compared with 21.633 ms when laying out every row.
The cached frame rendered nine rows and measured no new rows. The first pass
still took 102.0085 ms, compared with 106.3311 ms for the full-layout path. This
improves repeated rendering; a large result set still has an initial layout cost.

ASCII matching avoids lowercase-string allocations. The Unicode path preserves
the previous lowercase behavior, including multi-code-point mappings. A
controlled release benchmark over 20,000 ASCII chat lines measured a median of
1.6475 ms for lowercase copies and 1.3951 ms for the direct comparison. Both
found the same 4,000 matches. This is a comparison benchmark, not a whole-app
startup measurement.

## Subscriber messages and history

Subscription USERNOTICE messages now enter the ordinary chat pipeline with
their source ID, timestamp, sender, badges, and native emote positions. The UI
draws the notice and the subscriber's wrapped message. The normal publisher
writes the row to local history and retains the existing notification handling.

`ChatMessage::plain_text()` provides the complete text for searching, copying,
user-card summaries, and text exports. It handles current rows and older rows
whose text contains only the notice, without repeating the subscriber's body.
The stored schema remains compatible.

Ignored-phrase replacements keep the body and native emote offsets consistent.
Timeouts and chat clears affect nonempty subscriber messages in memory and in
history, including rows loaded after the moderation event. Empty notice headers
remain. A subscription cannot confirm a pending ordinary message merely because
its body matches that draft. Gift mentions compare the recipient's login, so a
localized display name does not hide a gift addressed to the current account.

## Live-update coverage

The 7TV worker assigns each topic to one connection and uses up to eight
connections. Each connection respects its announced subscription budget. Shared
sets use one subscription, and acknowledgments only apply on the owning
connection. Removing or replacing a scope retires its topics. Connection and
scope generations keep stale events and snapshots from replacing current data.

Metadata refreshes remain limited to four concurrent requests across the pool.
Scopes without live coverage receive periodic refreshes after the connection
grace period. Cosmetic fallback refreshes include definitions and user profiles;
a delayed catalog response cannot erase intervening live edits.

After a connection gap, cosmetic refresh waits for the affected subscriptions to
be acknowledged. This includes channel topics split across sockets. A snapshot
started before acknowledgment leaves a follow-up due, so it cannot hide an update
missed during the gap. A reader that exits without delivering its final event
still triggers recovery through task-completion monitoring.

The startup catalog task retries failed requests with exponential backoff, capped
at 30 seconds. Requests have a ten-second deadline. The task keeps one request
in flight, stops after success, and cancels when its receiver closes. Startup and
recovery requests share generation checks
so a late response cannot replace a newer catalog or live edit.

The loopback service exercises small announced budgets, shared topics, wrong
acknowledgments, removals, reconnects, source changes, stale events, and blocked
consumers. It also exercises the connection cap and fallback refresh path.

## Validation

The Windows release workspace run passed 1,421 tests with no failures; 22 tests
were ignored by the default run. The two controlled search benchmarks above ran
separately. The workspace correctness lint passed with the CI flags.

Commands:

```text
cargo test --workspace --all-targets --release --locked --offline --no-fail-fast
cargo clippy --workspace --all-targets --release --locked --offline -- -A clippy::style -A clippy::complexity -D clippy::correctness
```

Regression coverage includes full search-card geometry, short-window scrolling,
Unicode query replacement, popup keyboard ownership, cached-row geometry and
invalidation, subscriber messages through SQLite history, and real loopback HTTP
and WebSocket recovery. Linux CI also runs the workspace checks, builds the Debian
package, and checks the desktop and embedded webview under X11 before release.

## Signed-in Windows checks

The v0.5.8 baseline used the saved Windows account, its existing cache and
history, and a three-minute observation window. It recorded 617 UI updates:
median 0.7988 ms, p95 2.0612 ms, and maximum 6.2093 ms. No update reached 100 ms,
and the process remained responsive at each sample.

The first logged UI update was 0.3767 seconds after launch, authenticated IRC
connected at 0.6972 seconds, and the cosmetic catalog arrived at 1.7448 seconds.
These are logged milestones. They do not measure the completion of every image
download or GPU presentation. Live traffic, network responses, and cache contents
can differ between runs.

The packaged v0.5.9 candidate used the same saved account and a three-minute
observation window. It recorded 584 UI updates, remained responsive at every
sample, and produced no panic markers.

| Measurement | v0.5.8 baseline | v0.5.9 candidate |
| --- | ---: | ---: |
| Median UI update | 0.7988 ms | 0.7620 ms |
| p95 UI update | 2.0612 ms | 2.0233 ms |
| Maximum UI update | 6.2093 ms | 9.3737 ms |
| Updates at least 100 ms | 0 | 0 |
| First logged UI update | 0.3767 s | 0.3121 s |
| Authenticated IRC connected | 0.6972 s | 1.0898 s |
| Cosmetic catalog received | 1.7448 s | 2.0775 s |

These observations confirm that the candidate remained usable during startup.
They do not establish a startup speed improvement: traffic, history, images, and
network responses differed between the two runs.

Native checks confirmed that chat stayed below the inline card, the narrow
dialog scrolled to its regex diagnostic, Ctrl+F selected the existing query,
and global search retained Ctrl+F after Tab navigation. Escape closed the emote
picker after Tab navigation and returned to the composer. The main window's
size was restored after these checks.

## Remaining work

[Issue #37](https://github.com/frankischilling/crust/issues/37) tracks nested and
detached chat layouts, per-pane state, reply threads, richer message views, and
other remaining interface features. [Issue #17](https://github.com/frankischilling/crust/issues/17)
remains open for occasional long UI updates whose cause has not been reproduced
and attributed. A short responsive run cannot close that investigation.

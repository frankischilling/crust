# Chat recovery, previews, and image loading

This audit starts from v0.7.1 (`ee150b1`) and follows the fixes in
[PR #61](https://github.com/frankischilling/crust/pull/61). It covers sent-message
confirmation, reply identity, pane focus, draft recovery, keyboard dismissal,
preview parsing, and image work during startup and busy chats.
The final source revision is `ee3554c`.

## Sent messages and reply identity

[#57](https://github.com/frankischilling/crust/issues/57) records two related
problems. A local echo did not use the same display rules as incoming chat, and a
server confirmation could stop matching an echo after a rule replaced its text.
Changing the rule while a send was pending made the mismatch harder to recover.

Incoming messages and local echoes now share ignored-user checks, phrase actions,
nickname aliases, and mention detection. Native emote positions are inferred from
the original outgoing body before replacements adjust those positions. A hidden
local message stays hidden, while a failed send still retains its original draft
and reply for recovery.

Self messages retain the original body when a replacement changes the displayed
text. Both live reconciliation and saved-history confirmation compare that body.
This keeps one local identity and one saved row when the confirmation arrives,
even if the active replacement changed. Different original bodies remain distinct
when a rule makes their displayed text identical. Older history records without
the optional original-body field remain readable.

Reply context follows the parent ID actually addressed by the send. When a caller
provides only that ID, the local reply carries the ID without inventing an author
or message body. The renderer can find the exact retained parent by server ID; it
does not substitute a newer message from the same author. Missing parents receive
a neutral label. The composer uses the shared named Close control and avoids
blank names and trailing punctuation.

## Pane focus and recovered drafts

[#58](https://github.com/frankischilling/crust/issues/58) covers unread badges that
survived deliberate split selection and recovered drafts that remained out of
view. Split headers, pointer selection, pane shortcuts, and keyboard focus entering
a pane now acknowledge its unread and mention counts. The same acknowledgment
handles the Mentions feed and collapse to a single view. It does not reset the
pane's search, draft, or paused scroll position.

Choosing Restore brings the original composer into view and restores its draft.
This works for inactive channels, a background pane, a closed pane, a hidden draft in
an existing pane, and detached windows. A recovered overlay becomes interactive,
and a minimized detached window is asked to show and focus its composer.

Recovery still checks the original account, channel, and pane identity. It cannot
overwrite a newer draft or unrelated reply. If a pane or window limit prevents
reopening, the unsent message remains available. Reopening a hidden draft also
preserves the full closed-view history. Automatic recovery after a late rejection
keeps the current view and focus.

## Keyboard access and dismissal

Native testing found that Escape closed search and the emote picker but left
Settings open. [#62](https://github.com/frankischilling/crust/issues/62) tracks the
missing outer dismissal handler.

Opening Settings now focuses its search. An unused Escape closes Settings when
the window owns the keyboard, including after a title or body click. A nonempty
search clears first; shortcut capture cancels without changing the binding; the
expression editor discards its uncommitted expression; and menus dismiss
before either search or Settings. Focus in another window remains independent.
Closing Settings preserves the preferences it has already saved.

The same native pass found that the painted Resume scrolling control was absent
from the accessibility button list. [#63](https://github.com/frankischilling/crust/issues/63)
tracks its missing button metadata and focus treatment. It now exposes a named
button action and visible keyboard focus while preserving the existing return to
the newest messages and live scrolling.

## Preview parsing and thumbnail loading

[#60](https://github.com/frankischilling/crust/issues/60) covers preview failures
caused by treating the whole URL as a hostname and by slicing original Unicode
text with offsets from a lowercased copy. The latter produced a UTF-8 boundary
panic in a regression fixture. A fixed 512-byte tag window also truncated valid
metadata and could split a character.

Preview URL handling now uses parsed HTTP or HTTPS URLs. Hostname checks respect
domain boundaries, direct-image detection examines the path, and tweet proxy
rewrites change only the supported hostname. Paths, query strings, fragments, and
existing proxy URLs are preserved. Clip URLs receive the specific clip label.

The metadata scanner uses ASCII casing so offsets remain valid in the original
UTF-8 text. It reads complete tags, respects quoted greater-than signs, recognizes
actual attribute names, and accepts whitespace around assignments and unquoted
values. Relative image URLs resolve against the final response URL after
redirects. An empty or unsupported Open Graph image can fall back to a valid
Twitter image. This is a bounded metadata reader, not a complete HTML parser.

Titles and descriptions are published without waiting for a thumbnail. Visible
previews request image bytes through the existing UI image queue. The old direct
prefetch path was removed, avoiding duplicate requests and completion events when
the UI sees a missing thumbnail. Hidden previews do not trigger that image work.
The HTML prefix and oEmbed JSON readers both have a 64 KiB limit.

## Image validation and queued ownership

[#59](https://github.com/frankischilling/crust/issues/59) records two gaps in image
limits. Cache validation could start too many blocking decodes, and a successful
download released its slot as soon as it entered an event queue. The two queues
could therefore retain completed images while more downloads continued.

Validation now has two worker slots. A blocking worker owns its permit until it
finishes, even if the async caller is cancelled. The validator checks the larger
of the native decoded output and the predicted RGBA output, with checked
arithmetic and a 128 MiB limit per image. It returns dimensions with the original
encoded bytes, avoiding another header parse after a successful validation.

Disk and network cache misses use the same validator. Invalid disk entries are
removed and fetched again; fresh invalid responses are not cached. Existing
coalescing and cancellation cleanup remain in place. Validation covers a static
image or the first animation frame. It does not prove that every later GIF or
WebP frame is intact.

A successful image event now owns its download permit. Forwarded events and clones
share the permit. The slot is released when the last owning event is consumed or
dropped. Waiting completions thus share the existing 20-download limit. Failed
requests release the download slot before retry delays, and receiver closure
releases queued ownership.

These are separate budgets, not a total process memory cap:

| Work or data | Limit or ownership |
| --- | --- |
| Encoded HTTP response or disk image | Existing 32 MiB input limit |
| Initial image validation | Two blocking workers |
| Native or RGBA validation output | 128 MiB per image |
| Downloads and successful queued completions | 20 shared slots |
| Decoder scratch space, animation frames, UI caches, and textures | Separate decoder and UI limits |

## Regression evidence

Before the behavioral fixes, 17 regression tests reproduced the confirmation,
pane-focus, explicit-recovery, queued-image, and preview-parser defects. The
preview fixtures included the UTF-8 panic. Two automatic-recovery controls already
passed and continue to protect focus behavior.

Additional checks cover changed display rules with one persisted message row,
native-emote positions after Unicode replacements, hidden local messages with
failed-send recovery, old serialized history, exact reply parents, pane and window
limits, and a full closed-view history. Image tests exercise corrupt pixels,
output-size arithmetic, cancellation, queue forwarding and clones, receiver
closure, retries, and coalesced cache requests. Preview tests cover relative
images and metadata completion without extra image requests for HTML pages,
direct image links, and image content types.

Settings regressions cover initial focus and title activation, search clearing,
shortcut capture, expression editing, context menus, dropdowns, immediate reopening,
and another window retaining keyboard ownership.

The Resume scrolling regression exercises its accessible button action, Tab
focus, Enter, Space, and pointer clicks. It checks visible hover and focus states,
the final scroll position, and another pane retaining its paused state.

## Platform checks

The final source revision passed these checks:

| Check | Result |
| --- | --- |
| Windows workspace tests, all targets | 1,642 passed, 0 failed, 23 marked ignored |
| Ubuntu 24.04 workspace tests, all targets | 1,641 passed, 0 failed, 23 marked ignored |
| Windows workspace doctests | 1 passed, 0 failed |
| Correctness Clippy gate, Windows and Linux | Passed |
| Windows x64 release package, archive, and executable checks | Passed |
| Linux Debian package build | Passed |
| Linux embedded webview JavaScript IPC | Passed |
| Linux desktop startup under X11 | Passed |

The Windows test build used the release profile with `crust-ui` optimization set
to 1. Linux tests and the packaged binaries use the normal release profile. The
test-count difference is a Windows-specific UI test. Both correctness gates use
`-A clippy::style -A clippy::complexity -D clippy::correctness`; this does not claim
that every advisory Clippy warning has been removed. The 23 ignored tests are not
included in the passing totals.

[Linux CI for the source revision](https://github.com/frankischilling/crust/actions/runs/43790579323)
records the build, tests, package, and X11 checks.

## Native Windows observations

Release candidates were exercised with the saved account, existing cache, and
live chat. Checks covered a second pane and channel, unread acknowledgment through
pane navigation, a separate message-search window, an emote picker containing
2,216 entries, and opening and dismissing a reply banner. The picker loaded its
visible tiles; its rendering path requests only the visible row range.

The final optimized package was checked again for the two keyboard fixes.
Opening Settings focused its search. Escape closed Settings when the search was empty;
with a populated query, the first Escape cleared the query and restored the
previous settings page, and the second closed Settings. The native accessibility
tree exposed Resume scrolling as a named button. Giving that button keyboard
focus and pressing Space returned chat to the newest messages. The original
single-pane view was restored with an empty composer.

The startup comparison uses the first 180 seconds after each launch, before
interactive checks. Times to startup events are measured from the application's
startup log. UI durations measure the update function; they exclude event-loop
waiting and GPU presentation.

| Observation | v0.7.1 | v0.7.2 |
| --- | ---: | ---: |
| Recorded UI updates | 1,136 | 588 |
| First logged UI update | 0.240 s | 0.367 s |
| Embedded webview initialized | 0.423 s | 0.529 s |
| Authenticated chat connected | 1.526 s | 0.666 s |
| Last of three logged channel joins | 1.744 s | 0.912 s |
| 7TV cosmetics catalog loaded | 1.382 s | 1.476 s |
| Median UI update | 0.739 ms | 1.039 ms |
| 95th-percentile UI update | 1.427 ms | 2.721 ms |
| Largest UI update | 11.830 ms | 10.000 ms |
| UI updates of at least 100 ms | 0 | 0 |
| Event-budget overruns | 0 | 0 |

Both startup traces had no error or panic markers. The final package's longer
capture, including the keyboard checks, recorded 9,996 UI updates over about
7 minutes 15 seconds. Its largest update remained 10.000 ms, with no error or
panic markers. Decoded-image retention and reservations peaked at about 23.8 MiB
in that capture, below the separate 128 MiB decoded-image budget.

The results are mixed: the candidate connected sooner and had a lower maximum
update time, while its first update, median, and 95th percentile were slower.
Live traffic, retained history, cache contents, and network conditions differed
between launches. These samples do not establish a general speedup. The queue
and validation regressions verify the work limits independently of those timings.

An earlier candidate's picker and second-channel pass also showed why working set is not a
direct measure of decoded-image retention. The process reached about 765 MiB,
while the decoded-image counter stayed below 90 MiB. Runtime encoded bytes, UI
encoded bytes, decoded images, uploaded textures, derived static textures, and
painted-name textures have independent budgets. Closing a view does not empty
those caches; eviction responds to capacity pressure. This observation alone
does not establish a memory leak or account for every process allocation.

## Remaining performance investigation

The intermittent UI stalls in [#17](https://github.com/frankischilling/crust/issues/17)
remain open. Neither startup sample nor the native interaction checks reproduced
them. Fixing bounded queues and validation concurrency does not establish the
cause of an unobserved stall.

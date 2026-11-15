# Chat loading, emotes, and navigation

This audit followed messages from the network through saved history and the UI.
It also checked startup, provider failures, completion menus, and keyboard focus
in small windows.

## History and notifications

Mentions history previously read and decoded the full chat log before choosing
matching rows. It now uses a partial SQLite index for mention, highlight,
first-message, and pinned-message flags. Whisper rows and messages that suppress
notifications are excluded. The index is created on the background history
worker the first time it is needed; existing databases need no manual migration.

Incoming chat, sent messages, highlight changes, and whispers pass through one
bounded writer queue. Writes retain their order, and history reads wait for earlier queued
writes. Disk work runs outside the event reducer. The queue applies backpressure
instead of dropping accepted messages. Shutdown drains queued writes with a
three-second deadline and reports a failure if the disk does not finish.

Highlights and sent-message confirmations reach that writer through a separate
bounded queue. A full action queue, including plugin traffic, cannot discard
them. When storage falls behind, event intake waits for capacity while the UI
continues rendering. History reads wait behind those updates, and closing the
window hands its remaining backlog to the writer before shutdown.

Moderation now reaches both the channel buffer and Mentions. Exact message IDs
stay scoped to their channel. Timeouts and channel clears use the source event's
timestamp, which protects messages sent after the action. Without a source
timestamp, the action records only identities already present in memory or
saved history.

Deletion state is saved with existing rows, and a bounded rule table covers
messages written later. Remote history applies saved rules and preserves its
own deletion markers and clear records. A history response that overlaps live
messages carries its deletion flags into the retained copies before deduplication.

When a server confirmation replaces a local echo, its Mentions copy receives the
confirmed metadata without another unread count. Persistence reconciles the
exact local message ID, channel, and original timestamp in one transaction.
Deletion and highlight flags survive either write order, while other messages
with the same text remain separate. This prevents the unconfirmed saved row
from reappearing after the confirmed message has been deleted.

The writer also reconciles tracked local echoes before their server confirmations
enter the UI queue. Closing the window before the UI reads those confirmations
does not leave a second saved row. Tracking uses only local messages appended in
the current session, checks for an existing server ID before matching, and keeps
repeated text in send order. Later UI confirmation updates and highlights still
reach that exact row.

Local echoes use the successful send time while waiting for the server's
timestamp. Time spent in the outgoing queue cannot make a message appear older
than a channel clear that happened before it was sent.

Mention detection now checks word boundaries. A message addressed to
`alice_other` no longer counts as a mention of `alice`. Unicode letters and
combining marks remain part of a word. Nickname aliases apply only in their
configured channels, and live messages, whispers, and remote history use the same
whole-name matcher. Remote history also checks configured nickname aliases and
reply targets. Historical rows receive the current custom-highlight flags when
they load, so highlighted search results match those rows without replaying live
notifications. Restored deletion markers invalidate open search results even
when the channel's row count stays unchanged.

Native emote ranges that overlap, repeat, run backward, or exceed the message
are ignored where invalid. They cannot rewind the tokenizer and duplicate text.

## Provider recovery and image sizes

A failed channel provider retries after a 30-second cooldown. Only failed
providers are retried, successful empty inventories remain settled, and usable
catalogs remain available. Retries share the existing four-channel load budget.
Closing or reconnecting a room invalidates work from its previous generation.
Routine room-state updates cannot bypass the cooldown. Provider requests reuse
an HTTP client and its connection pool.

FFZ channel and global catalogs prefer animated assets when the provider includes
them. Normal and preview images fall back to a usable size from the same animated
set. Global loading includes only the declared default sets; other returned sets
do not replace ordinary text with emotes.

Normal-size 7TV images prefer the 2x asset. Larger images remain available for
previews and HD rendering. Selection keeps WebP animation when available and
excludes AVIF files that the current decoder cannot read. Image caches are keyed
by URL, so a previously cached 4x file does not replace a newly selected 2x file.

## Cache lifetimes

Completed link previews now retain at most 512 results, including empty failed
results. Active requests have a separate 64-URL limit. A request leaves that set
after publication, failure, or cancellation, so an evicted preview can load again
on a later hover. Generic HTML reads stop at 64 KiB; the old path downloaded the
entire response before applying that parsing limit.

Kick's inline emote definitions retain the 2,048 most recently observed entries
per channel. Fetched provider catalogs keep all their entries. The current
message supplies its own inline definitions during tokenization, and retained
native message spans keep their identities through later catalog refreshes.

## Layered emotes

7TV's active-set overlay flag now reaches provider catalogs, message spans,
live updates, chat, and whispers. The recognized BTTV global overlays use the
same renderer; a channel emote with the same name remains an ordinary emote.
A base emote and successive overlays occupy
one layout item. Horizontal whitespace between layers does not add width;
intervening text or a line break ends the group. An orphan overlay remains an
ordinary visible emote.

The renderer borrows the original spans, so copying and logging preserve every
code and separator. Hover previews include the layers. A change to the overlay
flag retokenizes existing messages and invalidates their layout, even when the
code and image URL stay the same. Mentions receives the same layout update.

Old saved spans default to ordinary emotes when they have no overlay field.
The existing five-field tokenizer callback remains supported; production paths
use the richer callback that includes the flag. Lua emote catalogs and spans
expose the new field as `zero_width`.

Ordinary and layered images use the image widget's animation deadlines. Static
WebP images no longer force a repaint every 33 milliseconds, and rendering no
longer scans image payloads on every frame to guess whether they animate.
The picker, completion menus, whispers, and hover previews follow the same pause
setting. Image signatures recognize animated content even when its URL has no
extension. Pausing animations still uses a cached first frame.

## Completion menus and search

Emote, command, plugin-command, and username menus share a scrollable layout that
stays inside the window. Rows adapt to the font size. The menu stays outside the
composer's full frame, including in short windows and at increased UI scale.
Pointer selection returns focus to the editor. Keyboard selection scrolls the
chosen row into view without undoing ordinary wheel scrolling. Escape closes
the menu while preserving the draft and editor focus. The menu stays closed
until the draft or caret changes. A changed filter selects the first match, and
controls covering the popup cannot activate its stale pointer target.

Virtual views such as Mentions and Live hide the channel stream header. Mentions
no longer exposes its internal channel identifier or waits for a stream status
that does not apply to that view.

Each Live row exposes a named accessibility action to open its channel. The
action covers the same full row as the pointer target, and navigation preserves
the existing composer draft.
The Live and Mentions sidebar controls expose their names and active state.

Channel changes defer composer focus until the editor is drawn. Accessibility
updates no longer create placeholder nodes for controls that have disappeared.
This fixes a crash when closing Settings with Windows accessibility active,
while preserving the draft and focus when returning from Live to a channel.
A pending handoff releases the previous editor immediately, so an earlier split
pane cannot receive text intended for the selected pane.

Command descriptions and provider labels remain readable on both ordinary and
selected rows. The composer uses a vector smile icon with the same focus,
hover, and accessibility behavior as the other controls.

Cross-channel search handles navigation keys only while its input or results
own focus. Clear and Close retain their keyboard actions. Search cannot consume
typing, Enter, or arrow keys intended for another control.

Plugin action calls now return `true` when queued, or `nil, reason` when the
queue is full or the background service has stopped. A callback no longer tries
to block a runtime worker when its command queue is full. Rejected actions are
not replayed automatically; the plugin can keep the input for an explicit retry.

## Text contrast

Plain username colors now adjust to the active theme when drawn in chat, Mentions,
and search results. Provider colors remain unchanged in stored messages and
caches. The display correction preserves hue and saturation while changing
lightness, including dark blue and purple on dark backgrounds and pale green on
light backgrounds. Custom name paints retain their existing compositing.

A bounded safeguard uses the [standard contrast calculation](https://www.w3.org/WAI/WCAG22/Understanding/contrast-minimum.html)
with a 4.5:1 target on the ordinary chat surface. Numeric checks of 8,192 sampled
color/theme pairs found no result below that target after RGB rounding.

Enabled navigation counts and Live metadata use secondary text normally and
primary text on selected or hovered rows. The Mentions header count also uses
secondary text. The dark-theme color pairs measured as follows:

| Label | Previous contrast | Corrected contrast |
| --- | ---: | ---: |
| Selected sidebar count | 1.20:1 | 8.29:1 |
| Ordinary Live metadata | 2.05:1 | 4.86:1 |

The shared light-theme style also gives ordinary bold text a dark foreground.
This keeps headings visible in Live, Mentions, settings, plugins, and tooltips.
The same foreground has 5.35:1 contrast against the existing pressed-button fill.

## Notification sounds

Audio device setup, sound-file reads, and decoder initialization previously ran
inside UI event handling. They now run on one lazy worker with space for eight
pending requests. UI submission never waits for that work. Settings and mute
changes cancel outdated requests, including sounds still being prepared. A
missing audio device is retried after a ten-second cooldown.

Windows desktop popups no longer add a separate system beep or toast sound.
The configured sound controller handles playback, volume, and muting, so a
desktop popup cannot bypass a disabled event sound. Settings previews retain
their explicit override of mute and event enablement.

A muted device probe measured a 410.0067 ms device opening on the worker while
the caller returned in 0.0402 ms. This is one device observation, not a general
latency benchmark. Fake-backend tests also hold preparation open while checking
submission, settings changes, queue limits, cancellation, and shutdown.

Slow-event diagnostics record the event's fixed name, item count, and elapsed
time. They do not format message contents or other event payloads.

## Controlled measurements

These measurements isolate the changed work. They are not whole-application
startup benchmarks.

### Mentions history

The release benchmark used an in-memory SQLite database containing 100,000
messages, of which 100 matched Mentions. Five alternating samples compared the
old full scan with the indexed query. The first indexed read includes creating
the index.

| Operation | Measured time |
| --- | ---: |
| Full scan and decode, median | 631.1574 ms |
| Indexed read, median after index creation | 0.3766 ms |
| First indexed read, including index creation | 207.6228 ms |

The regression suite also checks old databases, malformed and differently
formatted JSON, stable ordering, changes from another SQLite connection, and
the query plan. The query uses the attention index without a temporary sort.

### Animation dimensions

The release benchmark decoded a synthetic 24-frame GIF at 64 and 128 pixels,
with 15 samples at each size. This models the dimensions of a 2x asset and its
4x equivalent; it does not measure provider network latency.

| Dimension | Encoded bytes | Decoded RGBA bytes | Median decode time |
| --- | ---: | ---: | ---: |
| 64 px | 75,398 | 393,216 | 0.734 ms |
| 128 px | 154,448 | 1,572,864 | 2.735 ms |

For these dimensions, the smaller image uses one quarter of the decoded frame
memory. Actual savings depend on each provider's available sizes and formats.

## Interface verification

Native fixture runs covered chat, completion menus, the emote picker, whispers,
settings, search, and layered emotes in both chat and whispers. All eight runs
completed successfully. Visual review of 26 captures checked narrow and short
windows, enlarged text, message wrapping, popup boundaries, and layered hover
previews. The fixtures use synthetic messages and an offline account.

The final release-mode workspace run passed 1,250 tests across 20 targets, with
no failures and 16 optional tests ignored. Workspace correctness lints also
passed. The UI regressions include opening an already joined channel from its
Live row, keeping its draft, and opening the emote picker through both pointer
and accessibility actions.

Accessibility regressions also process consecutive frames through the native
tree consumer. They cover Settings and Whispers closure, the frame when their
controls vanish, and later settled frames, including focus and close actions
arriving together. Split-pane navigation, collapse, and picker insertion retain
their drafts and send subsequent typing to the selected editor.

The Windows archive includes both application executables. Its packaged webview
loaded offline HTML and returned the expected JavaScript IPC response.

A signed-in Windows run restored four channels. The three-second capture already
showed chat and emotes, and the app remained responsive at 3, 20, 60, and 180
seconds. The fixed 180-second observation after moving audio preparation recorded
2,769 UI updates: a 0.6647 ms median, a 1.1978 ms 95th percentile, and a 5.585 ms
maximum. None reached 100 ms, and the log contained no panic or slow-event warning.
During that run, audio device setup took 444.0712 ms on the worker.
The timing run preceded the final accessibility focus fix.

These timings measure the UI update function, excluding event-loop waits and GPU
presentation. Live traffic and cache state differ between runs, so the observations
do not establish a whole-application startup speedup. The signed-in checks also
opened the followed-channel Live list, saved Mentions, and existing channel tabs.

Native accessibility checks also closed the recovered-report dialog and Settings,
including an explicit focus action before closure, then opened Live, Mentions,
an existing channel, and the emote picker. The app stayed running and its log had
no new panic, error, or slow-event entry during those checks.

The final package also passed a light-to-dark theme check. Live and Mentions
headings and counts stayed readable, Settings closed after each theme change,
and the existing chat and emote picker remained usable without new log errors.

## Remaining investigation

[Issue #17](https://github.com/frankischilling/crust/issues/17) tracks occasional
long UI updates seen during populated-chat tests. The first 180 seconds of the
saved-account run recorded 5,513 updates, with a median of 0.6980 ms and one
428.6251 ms update. Event handling accounted for 427.7769 ms of that update.
This led to the audio work above and more precise event diagnostics. The
earlier update did not identify its individual events, so the device probe
does not establish that audio caused every previously observed stall.

Automatic echo tracking has a 512-message limit. If unconfirmed sends exceed it,
the writer falls back to the UI's exact confirmation updates for outstanding and
further sends during that session.
It does not evict older candidates and continue matching newer identical text.
This limit keeps memory bounded and preserves message identities when too many
confirmations are outstanding.

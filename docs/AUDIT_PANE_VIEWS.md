# Pane layouts, rich message views, and emote loading

This audit covers saved chat layouts, input shortcuts, reply threads, search and
user-card rendering, native global emotes, and queued cosmetic requests. It extends the
[search and live-update work](AUDIT_SEARCH_AND_LIVE_UPDATES.md).

## Pane layout and state

The old layout stored a flat list of at most four horizontal panes. A chat can
now split right or below, with nested dividers and a limit of 32 panes. The header
menu exposes both split directions and a reopen action; the same controls remain
available when a pane is too narrow for separate buttons.

Pane IDs stay attached to their drafts and scroll state when the layout changes.
Closing a pane stores its view state and position in a bounded session history.
Reopen uses the old sibling and split direction when they are still available,
or places the view beside the focused pane. Divider dragging uses the current
pointer position, which fixes continued movement while the pointer is held still.

Search fields and filter selections belong to a pane. Two views of one channel
can therefore use different queries and rules. Filter selections refer to saved
rule names; disabled rules remain disabled, and missing names are visible in the
selection dialog. Compiled rules are refreshed when the selection or saved rules
change.

Saved layouts include the tree, divider fractions, focus, and view settings.
Legacy flat layouts migrate to horizontal trees. Restoration removes unavailable
channels and invalid or repeated leaves while retaining the surviving views. If
only one view survives, its settings transfer to the ordinary single chat view.

View edits debounce settings writes. Shutdown captures the final layout, stops
the reducer, reloads the latest settings, and merges only the layout fields. An
immediate close before startup restoration leaves the previous layout intact.

Switching channels stores the previous view's unsent draft and reply context.
Returning to that channel restores its editor state. The same handoff applies
when a split pane changes channels or a layout collapses to the ordinary view.

## Message views and input controls

Global search and recent messages in user cards use the normal message renderer.
Their rows include emotes, badges, reply previews, subscription messages, links,
and context actions. Clicking a result body navigates to its source message;
sender names and links keep their own actions. Message identities remain stable
as earlier rows move out of view.

The search height cache includes layout-affecting message and image metadata.
Highlight-rule changes invalidate measurements so an offscreen banner cannot
leave later results at the wrong position. Keyboard navigation uses the result
rows without adding hidden stops for their body sensors.

Settings > Hotkeys has a Chat input category. Send, Send and keep text, history,
completion, Clear input, Cancel reply, and the emote picker can be rebound or
cleared. Conflict checks consider focus contexts and the default shifted Enter
alternatives. Rebinding Send removes its Shift+Enter alternative; the same rule
applies to Ctrl+Enter and Ctrl+Shift+Enter for Send and keep text.

Keyboard events keep their order around Send, Clear input, and navigation. If
Enter and more text arrive in the same update, the text after Enter starts the
next draft. Deferred input belongs to its original composer and channel, so
changing focus cannot apply it to another view's draft.

## Reply threads and draft recovery

The Open thread context action is available in chat, search, and user history.
Thread rows use the shared message renderer and retained channel history. Server
thread metadata connects nested replies to the original message. If the root is
missing, hover over Original message not loaded to inspect the available quote
context. An intermediate reply is not substituted for the root.

Each thread has its own composer and emote picker. Opening it leaves the main
chat draft and reply target intact. The window keeps its composer below the
scrolling messages and fits the application's minimum window size. Completion,
history, and picker shortcuts follow keyboard focus; Escape dismisses the active
input helper before closing the thread.

Live participation subscribes to a thread unless it was explicitly unsubscribed.
Subscribed replies enter the existing highlight and Mentions flow. Historical,
deleted, self, and notification-suppressed messages do not trigger notifications.
Thread views and subscription choices are scoped to an account and retained only
for the session, with limits of 64 views and 256 subscription entries. Opening a
thread does not request an archive of older replies.

Send and upload commands carry the originating composer identity. Delayed
results can therefore reach the correct view even when two panes show the same
channel. Rejections preserve the original body and reply context without
overwriting a newer draft. Upload links whose destination is unavailable remain
in the original account's unsent messages. The runtime also rejects a targeted
Twitch send if the connected account changed after the command was queued.

## Emote loading

Authenticated startup fetches Twitch's complete native global catalog. The
previous path could leave an account with only the 20 bundled globals when the
optional `user:read:emotes` permission was absent. Twitch's
[Get Global Emotes endpoint](https://dev.twitch.tv/docs/api/reference/chat#get-global-emotes)
accepts a user or app access token without that scope. Personal and follower
inventory continue to require their existing permission.

Global, account, and follower catalogs share one HTTP client and the existing
four-request budget. Successful global catalogs refresh after ten minutes;
failures retry after 30 seconds and preserve the usable catalog. The bundled
fallback cannot overwrite a successful catalog, including a valid empty result.
Account changes cancel obsolete requests, and late responses cannot publish into
the current session.

Queued 7TV user lookups recheck their generation after acquiring capacity and
before starting HTTP work. Shutdown can cancel either the wait or the request.
A controlled regression queues 64 obsolete lookups, changes generation, and
confirms that only the new request is polled.

## Verification

The Windows release workspace run passed 1,511 tests across 20 targets, with
22 opt-in performance tests excluded from the normal run. The 5,000-result search
benchmark was run separately. The workspace correctness lint gate also passed.

Interaction regressions cover nested geometry, divider dragging, duplicate
channel views, closed-pane restoration, channel drafts, async recovery, input
ordering, account changes, thread focus, compact windows, and drag-to-copy text
selection. Saved-layout tests cover migration and shutdown before the debounce
timer expires.

## Measurements

The controlled `benchmark_global_search_results_cache` fixture uses 5,000 rich
results. Two release-build runs on the same Windows machine measured:

| Path | First run | Verification run |
| --- | ---: | ---: |
| Cold cached layout | 162.67 ms | 120.88 ms |
| Cold full layout | 167.38 ms | 127.12 ms |
| Warm cached frame, median | 1.65 ms | 1.78 ms |
| Warm full layout frame, median | 58.24 ms | 41.20 ms |

The warm cached frame rendered seven rows and remeasured none. This checks that
the richer renderer retains visible-row processing. It does not compare the new
renderer against the previous plain-text previews.
The variation between runs is not evidence of an implementation speedup.

The first layout still measures every result to establish exact wrapped heights.
Progressive measurement with stable scroll anchors and keyboard jumps is tracked
in [#45](https://github.com/frankischilling/crust/issues/45).

A saved-account v0.5.9 native baseline recorded 602 UI updates: median 1.05 ms,
95th percentile 2.90 ms, maximum 8.98 ms, and no update at or above 100 ms. These
figures measure application update work, excluding presentation and compositor
time. They do not reproduce or explain the older intermittent stalls tracked in
[#17](https://github.com/frankischilling/crust/issues/17).

## Remaining work

Detached full-chat windows and transparent overlays still need a reusable chat
surface with independent drafts, focus, picker state, geometry, and input routing.
They remain open in [#37](https://github.com/frankischilling/crust/issues/37).
The existing detached search window is a separate feature.

The historic long-update report remains open until a trace identifies its cause.
A short native session without a stall is insufficient to close it.

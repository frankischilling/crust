# Chat windows, search, and recovery

This audit follows the v0.6.0 pane and message-view work. It adds detached chat
windows, removes the full-list layout cost when opening a large search, and fixes
badge refreshes, announcement handling, and recovery controls.

## Detached chats and overlays

The chat header menu offers Open in new window, Open overlay, and Manage chat
windows directly. Each chat window uses the normal message renderer and composer,
with its own draft, reply, search, filter selection, emote picker, and scroll
position. Up to 16 windows can be open, including multiple views of one channel.

The flat header menu sizes to its action labels and stays within the available
viewport. The manager labels the opacity slider Background. Shared icon controls
expose button actions and the current state of toggles to accessibility tools.

Window geometry, search, filters, and overlay settings are saved. Drafts and
recently closed windows stay available during the session. Closing the main tab
keeps its detached views connected; closing the last view leaves the channel.
The Chat windows manager can restore a tab, reopen a closed window, or reset an
offscreen window beside the main window.

The reopen history retains 16 closed windows. When an older window leaves that
history, its remaining draft moves to unsent recovery. Virtual feeds cannot be
detached, and startup restores windows only for channels eligible to join.

Overlays have adjustable background opacity and an explicit Click-through mode.
The manager's Interact action disables mouse passthrough and focuses the overlay.
Reset position also restores interaction, size, and visibility.

The native renderer requests a transparent-capable visual at startup because all
windows share it. The main window still paints an opaque background. On Windows,
the patched OpenGL backend preserves requested child-window transparency when
the shared configuration has alpha bits. This avoids a legacy WGL capability
check that disabled transparency before window creation and prevented the
window's DWM transparency setup. Overlay transparency requires support from the
display driver and desktop compositor.
On Wayland, the compositor controls window placement, stacking, and focus; the
application's position, always-on-top, and focus requests may be ignored.

Ctrl+W closes the focused chat view and Ctrl+, opens settings. Both appear in
Settings > Hotkeys. Detached windows keep local search and input shortcuts;
quick switch, cross-channel search, and settings bring the main window forward.

Returning focus to the composer after using its picker or search keeps the same
native window. Focus requests leave the window's creation settings unchanged.

Crust includes a patched copy of eframe 0.31.1 in `vendor/eframe`, selected through
Cargo. Both native backends initialize each window's AccessKit adapter once,
before showing or maximizing the window, when accessibility is enabled. A
replacement native window receives a new adapter; ordinary focus changes reuse
the existing one. The Windows transparency fix is part of the same patch.

Windows release builds request the GUI executable subsystem. The webview helper
starts with `CREATE_NO_WINDOW` while retaining its piped protocol and diagnostic
output. These changes address the extra console windows created on launch.

Delayed send failures and image uploads retain their originating composer and
account. A result cannot replace a different window's draft or cross an account
change. Closed windows retain the same ownership checks. Emote and filter changes
also refresh detached-only channels and recently closed views.

## Search layout

Previously, the first display of rich results measured every matching message,
even when only a few rows were visible. Search now estimates unseen heights and
measures rows as they enter the viewport. Width, font, image, highlight, and
message changes invalidate measurements without forcing the whole list to render.

Keyboard jumps measure a bounded neighborhood around the selected result. The
visible message acts as an anchor when results are inserted, removed, or
remeasured. Result bodies, sender names, links, and message menus retain their
normal actions.

Local search popups bound all three fields to the available content width. Help
and regex errors wrap, and the vertical scrollbar appears only when the form
exceeds the window height.

## Badge catalogs

Bundled badges fill missing cache entries without replacing newer cached URLs.
A refresh stops after the first valid authoritative response. A successful
authenticated request therefore skips both fallback requests; anonymous refreshes
start with an unauthenticated source. A valid empty response replaces the global
scope, while total failure preserves the usable catalog. Channel badges remain
separate, and obsolete refresh generations cannot publish late results.

## Announcements

Live and historical announcements keep their sender, badges, emotes, message ID,
timestamp, and notice color. Shared announcements retain source-channel metadata.
They participate in moderation, user history, chatter tracking, analytics, and
late emote-catalog updates. Search accepts `is:announcement`; expression filters
expose `flags.announcement`.

Lua message events expose `msg_kind.type = "Announcement"` and a lowercase
`msg_kind.color`. Other message metadata retains its existing shape.

Older saved system-notice rows cannot recover metadata discarded by the original
parser. The richer form applies to new messages and raw history records.

## Recovery and small windows

Crash reports and IRC status now fit the application's minimum window size.
Scrolling report content leaves Close and the More menu accessible. A failed
deletion keeps the report and selection available for retry; Dismiss all retains
any files that could not be removed.

Restart Crust requests a normal application shutdown. The app saves layout and
settings, flushes history, stops the runtime, and clears the session sentinel
before launching the replacement process. The report widget no longer exits the
process itself.

## Native Windows checks

Signed-in checks used fresh processes from packaged Windows builds at 125%
display scaling:

- The desktop was visible behind overlay messages in a fresh packaged process,
  without external DWM calls. Click-through, Interact, Reset position, closing,
  and reopening worked.
- Restart restored a maximized chat, saved window geometry, search, and overlay
  settings.
- Windows UI Automation found controls in native child windows and could focus
  the detached chat's composer directly. Closing its emote picker with Escape
  retained the same native window handle.
- Local search fit a 480 × 300-pixel physical window. Cross-channel search and
  settings shortcuts from a child chat brought the main window forward.
- The header menu fit its labels in a 1280 × 800-pixel window. Windows UI
  Automation invoked Split options as an ordinary button action, then toggled a
  child window's picker from Off to On and back to Off. Its visible highlight
  matched the open state, and the window handle stayed unchanged.
- The overlay manager exposed its opacity slider with the name Background.
- The webview helper initialized during the diagnostic capture. Both processes
  shut down normally afterward. A subsequent desktop launch reopened the app and
  helper without visible console windows.
- The four original main tabs remained. Test drafts were cleared, and the
  application shut down normally.

A later diagnostic startup capture confirmed that all four saved channels
reached the outbound JOIN path. One received a channel-level server rejection.

Native checks used the compiled OpenGL backend on Windows. The optional wgpu
changes received source review only and were not independently compiled or run.
Wayland runtime behavior was not tested. The report viewer only opens when
reports exist; this session did not exercise its Restart Crust button.

## Startup observations

A 180-second saved-account capture used the packaged build from `07702a7`.
No interactive checks or Cargo builds ran during the sample. Windows reported
the process as responding at all four checkpoints, taken around 3, 20, 60, and
180 seconds.

| Observation | Result |
| --- | ---: |
| First logged UI update | 0.528 s |
| Webview helper initialized | 0.656 s |
| Twitch authentication completed | 0.699 s |
| Native global emote catalog updated | 0.991 s; 310 emotes |
| 7TV cosmetics catalog received | 1.922 s |
| Recorded UI updates | 688 |
| Median UI update | 1.711 ms |
| 95th-percentile UI update | 4.275 ms |
| Maximum UI update | 11.473 ms |
| Updates of at least 100 ms | 0 |
| Event-budget overruns / panic markers | 0 / 0 |

At 180 seconds, the app used about 331 MiB of working memory and 289 MiB of
private memory, with 6.22 seconds of process CPU time. UI update timings exclude
presentation and compositor work. This is one observation with an existing
cache and saved profile. Earlier captures used different builds and sessions;
these results do not establish a startup improvement or explain the older stalls
tracked in #17.

## Search measurements

The controlled Windows release fixture contains 5,000 rich results in a
480 × 320-point viewport. Each path has one cold frame and four warm-up frames,
followed by 25 timed warm samples, alternating cached and full rendering. The
baseline uses the v0.6.0 main commit `286ee25`; the candidate uses this release's
progressive layout.

Run the candidate fixture with:

```sh
cargo test -p crust-ui --release --locked benchmark_global_search_results_cache -- --ignored --nocapture
```

| CPU layout measurement | Baseline | Candidate |
| --- | ---: | ---: |
| First cached layout | 148.0738 ms | 4.2542 ms |
| First full layout | 142.2043 ms | 160.9927 ms |
| Warm cached median | 1.2628 ms | 1.6202 ms |
| Warm full-layout median | 39.6838 ms | 52.2257 ms |

The candidate's first cached frame rendered and measured seven rows. Warm cached
frames rendered seven rows and remeasured none. The cold cached result is about
35 times faster on this fixture. Warm times varied with the full-layout control;
these measurements do not establish a warm-path improvement. They measure CPU
layout, excluding presentation and compositor time.

## Automated checks and packaging

For source revision `07702a7`, the Windows workspace run passed 1,571 tests, and
the doctest run passed one test. The ordinary run skipped 22 opt-in benchmark and
fixture tests; the search benchmark above was run separately. To fit the host's
available memory, the workspace and doctest commands used the temporary setting
`--config profile.release.package.crust-ui.opt-level=1`. Release correctness
linting passed with the normal release profile.

The Windows package for `07702a7` used the normal optimized release profile
(`opt-level=3`) without the test override. Verification passed for the archive,
all four payload files, ZIP CRCs, the checksum sidecar, and the x64 executable
architecture. The main executable uses the Windows GUI subsystem; the helper
retains its console subsystem and starts with `CREATE_NO_WINDOW`.
Linux [CI run 43754686032](https://github.com/frankischilling/crust/actions/runs/43754686032)
passed workspace tests, correctness linting, Debian packaging, both X11 startup
checks, and package upload for source revision `07702a7`.

Focused checks covered 29 search cases and 20 detached-window cases. Search
regressions verify distant keyboard jumps, partial-row anchors during resize,
insertion and removal, and a click on the preserved result in the immediate next
input frame. Runtime checks cover authoritative badge request counts, fallback
credential isolation, rich announcement history, moderation, and late catalogs.

## Remaining work

[#17](https://github.com/frankischilling/crust/issues/17) tracks older intermittent
long UI updates. A responsive test session does not explain those earlier stalls.
Kick and generic IRC remain beta features. Drafts and closed-window history
remain session-only.

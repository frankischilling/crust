# Chat state, channel banners, and image loading

Date: 2026-11-15. Baseline: `3c25f74` (v0.7.0).

This pass follows the [chat-window audit](AUDIT_CHAT_WINDOWS.md). It covers
[compact banners](https://github.com/frankischilling/crust/issues/51),
[confirmed message state](https://github.com/frankischilling/crust/issues/52), and
[image decoding](https://github.com/frankischilling/crust/issues/53). Final review
also covered the [transparent header gap](https://github.com/frankischilling/crust/issues/55)
and [informational text contrast](https://github.com/frankischilling/crust/issues/56).

## Confirmed messages and moderation roles

An optimistic sent row kept its local identity when the server confirmed it, but
the update copied only part of the server record. First-message and pinned flags,
reward metadata, and Shared Chat information could be lost. Confirmation now
imports that metadata while preserving the local ID, history classification, and
accumulated deletion, highlight, mention, and notification-suppression state.

Matching also includes the Shared Chat source room. A mirror with the same sender,
text, and reply target cannot consume a pending local send from another source.
The history writer uses the same predicate, keeping the two durable rows separate.

Mentions updates an existing row using the confirmed identity. If the server
record first makes the message eligible, the UI adds the missing row without
another unread or notification. A duplicate confirmation does not add another row.
Suppressed messages remain excluded from that insertion path.

Moderator and lead-moderator badges now produce consistent role state for both
the UI and the outgoing rate limiter. VIP status still raises the send allowance
without enabling moderator controls.

## Channel banners and startup

Hype Train and Shared Chat headers used competing horizontal labels, and raid
text could crowd out the dismiss glyph. The new layout subtracts the complete
frame margin, wraps metadata, and bounds participant labels. Tooltips retain full
names and counts. Raid dismissal uses the shared vector button and exposes its
label and activation action to accessibility tools.

Room-mode pills can occupy more than one row. Their panel grows with the labels
so they do not overlap the chat beneath it. Closing a channel before its startup
work finishes removes that target from the loading state.

The baseline failed three production-widget regressions: Hype Train and Shared
Chat content extended outside a 150-point pane, and the raid dismiss control had
no named accessibility action. Existing banner tests still passed in that run.

## Header opacity and readable labels

The native main window shared a transparent framebuffer clear with detached
overlays. Individual panels painted their own backgrounds, leaving a transparent
seam beneath the toolbar. A production-rendering regression reproduced a fully
transparent sample at that boundary in a 696-point window. The main viewport now
paints an opaque theme background before its panels. Detached overlays retain
their transparent clear and configurable background opacity.

Informational text now adjusts against its painted background while preserving
the accent used for borders, fills, and progress bars. This covers room modes,
channel points, stream status, pinned messages, Hype Train and raid text, and
Shared Chat participants. Stream titles, loading labels, and helper text use the
readable secondary palette. Offline participant names remain distinct without
fading into the banner.

Several translucent selection, hover, and pill colors used straight RGB channels
with a premultiplied-alpha constructor. They now use the unmultiplied constructor,
so the requested opacity determines the fill instead of adding excess color.

The contrast regression checks explicit light and dark surfaces, tinted
backgrounds, and mid-tone surfaces without changing process-global theme state.
It requires a foreground contrast ratio of at least 4.5:1 and preserves colors
that already meet that threshold. That check applies to opaque theme surfaces;
the desktop behind a translucent overlay can change its visible contrast.

## Static images and animation timing

PNG and JPEG requests previously fell through to a synchronous decoder. They now
use the same bounded worker queue as animated and paused images. Pending requests
share one decode; retained pixels remain usable after raw bytes are evicted.
Cancelled jobs release their reservations when the work exits. Stale completions
cannot repopulate forgotten entries, and unsupported sources can still reach a
custom image loader.

Queued and running jobs share a 128 MiB encoded-data budget across loaders. A
cancelled job keeps its source bytes charged until it is consumed or dropped;
cache eviction cannot release that charge while the job still owns the data.
Admission retries without blocking the UI when the budget is full. A source
larger than the entire budget fails explicitly. Decoded-image and texture budgets
remain separate; these limits do not measure total process memory or decoder
scratch allocations.

Static name-paint sampling and ordinary image rendering share decoded pixels.
Paused animations retain their separate first-frame cache. Normal GIF and WebP
widgets continue to use indexed animation frames.

Frame delays of at most 10 ms are normalized to 100 ms; other delays below 20 ms
are raised to 20 ms. Tests exercise zero and short delays in both formats and
check that actual image widgets advance through their frames.

## Cold static-image comparison

Seven alternating trials used fresh image contexts and the same 1024 × 1024 PNG
(3,024,991 encoded bytes). The comparison ran the opt-in
`perf_static_image_cold_load` test from the workspace-built UI test executable,
with UI code at optimization level 1 and dependencies at level 3.

| Median measurement | Synchronous control | Background route |
| --- | ---: | ---: |
| First texture request | 14.1329 ms | 0.0631 ms |
| Total image readiness | 14.1330 ms | 13.7211 ms |

The measured gain is reduced blocking in the initial UI request. Total readiness
stayed around 14 ms. The small sample and 1 ms readiness polling do not establish
a reduction in total decoding time. The control uses the default static decoder
without the former bounded-wrapper bookkeeping; the background case uses the
production bounded loader. Encoded bytes start in memory, and the test excludes
network fetches and GPU upload.

## Baseline observation

A 180-second Windows run used the saved account and existing cache. The app
authenticated in 0.66 seconds and updated the 310-entry native global emote
catalog in 0.92 seconds. It recorded 957 UI updates: median 1.06 ms, 95th
percentile 2.49 ms, and maximum 8.07 ms. No update reached 100 ms; there were no
event-budget overruns or panic markers. The process responded at all four
checkpoints.

Three restored rooms received server join acknowledgments. Another saved room
did not join, so this sample does not establish successful loading of every saved
channel. UI timings exclude event-loop waits and GPU presentation. Traffic and
cache state vary between live runs, so they are observations rather than a
controlled overall startup comparison.

The earlier intermittent stalls remain open in
[#17](https://github.com/frankischilling/crust/issues/17); this sample did not
reproduce them.

## Automated checks

Code revision `d084c8b` passed the following checks:

| Check | Result |
| --- | --- |
| Windows workspace tests, all targets | 1,594 passed; 23 ignored |
| Windows workspace doctests | 1 passed |
| Windows correctness lints | Passed |
| Windows release package | Built; SHA256, ZIP integrity, and copied payload verified |
| Linux workspace tests, all targets | 1,593 passed; 23 ignored |
| Linux correctness lints and Debian package build | Passed |
| Linux desktop startup under X11 | Passed |
| Linux embedded webview under X11 | JavaScript IPC received |

The Windows test build uses release dependencies with the UI crate at optimization
level 1. Correctness linting uses the normal release configuration. The 23 ignored
cases are opt-in checks and are excluded from the passing test counts.
[Linux CI run 43772489098](https://github.com/frankischilling/crust/actions/runs/43772489098)
records the normal release-profile checks and package upload for this revision.

The Windows package uses the normal release optimization level. Its two
executables match the build output byte for byte and declare PE32+ x86-64. The
archive also contains the matching license and README, and its extracted copy
matches every packaged file.

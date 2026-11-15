# Crust v0.7.0

Open chats in separate windows or transparent overlays, each with its own draft,
search, filters, and scroll position. Large searches now measure visible results
first. This release also fixes badge refreshes, rich announcements, and recovery
controls in small windows.

## Chat windows

- Open the chat header menu and choose Open in new window or
  Open overlay. Up to 16 chat windows can be open. The flat menu fits its action
  labels within the available window space.
- Each window has an independent composer, reply target, emote picker, search,
  filters, and scroll position. Delayed uploads and send failures return to the
  originating composer with the original account checks.
- Window size, position, search, filters, and overlay settings survive a restart.
  Drafts and recently closed windows remain available for the session.
- Closing a channel's main tab keeps detached views connected. The Chat windows
  manager can restore the tab, reopen a closed window, or reset an offscreen one.
- Overlays provide Background and Click-through controls. Background adjusts
  opacity; use Interact in the manager to regain keyboard and mouse control.
- Ctrl+W closes the focused chat view and Ctrl+, opens settings. Both shortcuts
  can be changed in Settings > Hotkeys. A detached window's close shortcut leaves
  the main chat view open.
- Returning focus after using an emote picker or search keeps the same native
  chat window. Native child windows initialize accessibility support before
  they are shown.
- Windows overlays keep the requested transparency when their native windows
  are created with an alpha-capable graphics configuration. These native window
  fixes use a patched copy of eframe 0.31.1 included in the repository.
- Shared icon controls expose button actions and the current state of toggles
  to accessibility tools.

## Search and loading

- Large rich searches measure visible rows first, then refine unseen row heights
  as you scroll. Keyboard jumps and changes to results preserve navigation and
  the visible message position.
- Local search fields fit the popup width. Help and regex errors wrap, and the
  vertical scrollbar appears only when needed.
- Bundled badges no longer overwrite newer cached entries. Badge refreshes stop
  at the first valid response, avoiding two fallback requests after primary
  success. Failed refreshes retain usable catalogs.

## Messages and recovery

- Announcements preserve the sender, badges, emotes, timestamp, message ID, and
  notice color in live chat and history. Shared announcements retain their
  source metadata. Moderation, user history, analytics, and late emote updates
  recognize the new message kind.
- Use `is:announcement` in search or `flags.announcement` in expression filters.
  Lua message events expose the `Announcement` kind and its lowercase `color`.
- Crash reports and IRC status fit small windows. Failed report deletions remain
  available for retry, and the recovery dialog keeps its closing controls visible.
- Restarting from the recovery dialog now saves through normal shutdown before
  launching the replacement process.
- Windows release builds request the GUI executable subsystem, and the webview
  helper starts without creating a console window.

## Validation and performance

For source revision `07702a7`, Windows checks passed 1,571 tests plus one doctest,
with 22 opt-in benchmark and fixture tests skipped. The workspace and doctest
commands used `--config profile.release.package.crust-ui.opt-level=1` to fit the
host's available memory. Release correctness linting passed with the normal
release profile. Focused checks covered 29 search cases and 20 detached-window
cases.

The Windows package for `07702a7` was built with the normal optimized release
profile (`opt-level=3`), without the test override. The archive, all four payload
files, ZIP CRCs, checksum sidecar, and x64 executable architecture passed
verification. The main executable's Windows GUI subsystem was also verified.
Linux [CI run 43754686032](https://github.com/frankischilling/crust/actions/runs/43754686032)
passed all six gates for source revision `07702a7`.

Signed-in Windows checks showed the desktop behind overlay messages in
a fresh packaged process without external DWM calls. They also confirmed overlay
interaction and reopening, restored window settings after restart, child-window
accessibility, direct composer focus, and the same native window after closing
its picker. Local search fit a 480 × 300-pixel physical window at 125% display
scaling, and child shortcuts opened cross-channel search and settings. The four
original main tabs remained; test drafts were cleared before normal shutdown.
The compact menu fit its labels, its action worked through Windows UI Automation,
and the child picker reported Off, On, then Off with matching visible state. The
overlay manager's opacity slider exposed the Background label.
Both processes shut down normally after the diagnostic capture. A subsequent
desktop launch reopened the app and helper without visible console windows.

Native testing used OpenGL on Windows. The optional wgpu changes received source
review only and were not independently compiled or run; Wayland remains untested.
The recovery dialog's Restart Crust button, which is available only when reports
exist, was not exercised.

On the controlled 5,000-result fixture, first cached layout fell from 148.07 ms
to 4.25 ms, with seven rows measured. Warm cached layout had a 1.62 ms median.
Warm timings varied with the full-layout control, so these results establish the
cold-layout improvement without claiming a warm-path speedup.

The final 180-second saved-account capture recorded 688 UI updates: median
1.71 ms, 95th percentile 4.28 ms, and maximum 11.47 ms. No update reached 100 ms;
there were no event-budget overruns or panic markers. Twitch authentication
completed in 0.70 seconds, and the native global catalog updated with 310 emotes
in 0.99 seconds. Windows reported the process as responding at all four
checkpoints. This is one cached-session observation, not a controlled startup
comparison; UI update timings exclude presentation and compositor work.

The [chat-window audit](https://github.com/frankischilling/crust/blob/v0.7.0/docs/AUDIT_CHAT_WINDOWS.md)
records implementation details and validation. The older intermittent update
stalls remain tracked in [#17](https://github.com/frankischilling/crust/issues/17).

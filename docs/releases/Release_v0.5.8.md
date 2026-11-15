# Crust v0.5.8

7TV name paints now appear in ordinary chat, Mentions, search results, reply
previews, inline mentions, and received-whisper headers when the sender's
cosmetics are known. Gradients and image paints are drawn across the letters,
with transparent colors and layered shadows.

## Name paints

- Linear and radial gradients support direction, repeating stops, and hard color
  boundaries. Image paints preserve detail inside each letter and follow the
  animation preference.
- Transparent paint colors reveal the sender's base color. Names keep a readable
  fallback while an image is loading or unavailable.
- Paint coordinates follow the name even when timestamps or badges precede it.
  The colon keeps its ordinary text color.
- Shadow-only definitions are accepted. Chat settings control paints, shadows,
  and large shadows separately, retaining child preferences when paints are off.
- Live paint and badge assignments update retained recent messages. Reconnects
  refresh recent senders first, and an older profile response cannot overwrite a
  newer live selection. Pending refreshes survive closing their original room.
- A live entitlement change that interrupts an initial profile load schedules its
  replacement, so the sender's remaining cosmetics and personal emotes still load.
- A shared lookup keeps linked names consistent across views. Explicit removals
  survive older message snapshots, and account renames do not transfer a paint to
  another user.

## Loading and rendering

- Image paints and paused emotes share the bounded background image decoder.
  First-frame decoding no longer runs synchronously during UI rendering.
- The first frame can load within its own allocation budget even when the full
  animation is too large. RGB16 and RGBA16 PNGs reserve enough memory for decoding
  before conversion to display pixels.
- Hidden views retain active requests and retry cooldowns. Old decode jobs cannot
  publish into a replacement source, and retained still frames share image data.
  Reloading an evicted decoded image rechecks the shared memory limit.
- Painted text reuses bounded raster and texture caches. Cold work is spread
  across frames when many distinct names appear together.

## Chat controls

- Split menus use the shared vector icon and remain reachable in narrow panes.
  Search, Close other splits, and Close stay available when inline controls do
  not fit. Right-clicking the title opens the same menu.
- Wrapped message bodies use the full pane width after a long sender name or
  badge prefix. This applies to ordinary and suspicious-user messages and keeps
  measured row heights consistent with the text drawn on screen.
- Ctrl+R searches the focused composer's sent-message history. Ctrl+R or Up moves
  to older matches; Ctrl+Shift+R or Down moves to newer matches. Enter, Tab, or the
  Use message button copies a match into the composer. Escape or Cancel preserves
  the original draft.
- Painted names in Mentions retain message navigation and the context menu.
  Search rows keep their wrapping, click target, and keyboard focus.
- Reply previews constrain long names to the pane. Cached sender lookups avoid
  rescanning retained history for each visible reply.
- Confirmed local echoes refresh row layouts and reply identities when server
  metadata replaces the pending message.

The [paint and chat audit](../AUDIT_NAME_PAINTS.md) records validation, measurements,
and remaining feature gaps. The investigation into occasional long UI updates
continues in [issue #17](https://github.com/frankischilling/crust/issues/17).

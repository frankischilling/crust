# Crust v0.5.5

This release fixes stale user-card results and missing controls on Live and
Mentions splits. It also removes credentials from settings diagnostics and
reduces repeated badge requests and Mentions layout work.

## Chat controls

- User cards accept external logs only for the current user, channel, and
  refresh. Closing or reopening a card invalidates earlier results. A rejected
  request or lost runtime ends the loading state and allows a retry.
- Live and Mentions splits have readable headers and close controls. Closing
  one preserves the remaining chat and its draft. These views omit channel
  search controls that they cannot use.
- Live rows adapt to narrow panes and enlarged text. Viewer counts and uptime
  move below the login, thumbnails yield space, and joined status stays readable.
- Malformed Unicode color values produce a parse failure instead of a panic.

## Diagnostics and badge loading

- Settings summaries in crash reports exclude credentials and free-form values
  such as account labels, paths, headers, and commands. Malformed settings errors
  retain the error location without copying source text or rejected values.
- Badge loads reuse one HTTP client and the current validated account credentials,
  avoiding a separate token validation for each load. Public fallback requests
  receive no authorization headers.
- Account changes, logout, disconnect, and revoked authentication cancel pending
  channel and global badge loads. A new global refresh replaces the previous one.
  Bundled global badges load once during startup.
- Permanent HTTP client errors stop retries for that URL. Timeouts, rate limits,
  transport failures, and server errors retain bounded retries, and alternate
  public endpoints remain available.

## Mentions performance

Mentions reuses measured row heights and skips layout for unchanged rows outside
the viewport. The bounded cache refreshes when content, width, fonts, or layout
settings change. Visible rows keep their current colors, moderation state, and
links to the source message.

A release-profile Windows benchmark compared full rendering with the cached path
using 2,000 synthetic wrapped messages in a 480 by 320 point viewport. Both paths
were warmed before measurement; five rows were rendered in the cached viewport.

| Measurement | Full rendering | Cached rendering |
| --- | ---: | ---: |
| Median frame time | 12.843 ms | 0.460 ms |
| p95 frame time | 14.115 ms | 0.595 ms |

These measurements cover warm egui rendering, excluding network work and GPU
presentation. They do not measure the frame rate of the whole application.

The [audit and measurements](../AUDIT_STABILITY.md) describe regression coverage
and native checks. [Issue #17](https://github.com/frankischilling/crust/issues/17)
remains open; its original reproduction still needs a separate check.

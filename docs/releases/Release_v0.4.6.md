# Crust v0.4.6 Release Notes

Date: 2026-04-08

v0.4.6 focuses on updater compatibility and release metadata alignment.

## Highlights

- Expanded Linux updater support messaging and detection from "Debian Linux" to Debian-based distributions.
- Added explicit Debian-family detection for common derivatives (including Linux Mint, Pop!_OS, and Kali) using `ID` and `ID_LIKE` values from `/etc/os-release`.
- Fixed settings UI copy and gating so updater controls are shown on Linux builds (not only on Windows builds).
- Added updater unit tests for Debian-family distro detection and os-release parsing behavior.
- Workspace and internal crate versions bumped to 0.4.6.

## Updater Behavior Notes

On Linux, auto-update checks/install remain gated to Debian-family distributions and require:

1. A matching artifact name format: `crust-v<version>-debian-<arch>.deb`
2. Architecture compatibility with the running build
3. A valid asset digest in release metadata (`sha256:<hex>`)

Installer handoff continues to use desktop opener fallbacks (`xdg-open`, then `gio open`).

## Notes

- Stable-only release checks remain in effect (draft and prerelease tags are ignored).
- Unsupported distributions still fail safely with user-facing updater messages.
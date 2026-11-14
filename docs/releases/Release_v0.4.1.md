# Crust v0.4.1 release notes

Date: 2026-04-06

v0.4.1 adds a Windows updater that downloads releases from GitHub, verifies
their SHA256 digest, and installs them on restart after user confirmation.

## Highlights

- Windows auto-update support from GitHub Releases
- SHA256 verification before install
- Prompt-first update flow (no silent install)
- Update checks at startup and every 24 hours
- Settings UI controls for update behavior and install actions
- Current app version shown directly in the Updates settings section

## Windows updater

The updater currently supports Windows only.

When update checks are enabled, Crust:

1. Queries the latest stable release on GitHub.
2. Compares the latest release version against the running app version.
3. Looks for a Windows x64 zip artifact in the release assets.
4. Validates the downloaded artifact against the release SHA256 digest.
5. Stages the update, then applies it during restart using a PowerShell installer step.

The update install flow is user-initiated from the UI and requires confirmation.

## Settings and controls

In Settings > Integrations > Updates, you can now:

- Enable or disable automatic update checks
- Manually run Check Now
- Install an available update and restart
- Skip a specific offered version
- Open the release page in browser
- See the current running version

## Release packaging requirements

To remain compatible with the updater, GitHub releases should include:

- A Windows x64 zip asset (name includes windows and x64)
- A valid SHA256 digest for that asset

If either requirement is missing, Crust will report that the update cannot be installed automatically.

## Notes

- Auto-install is currently Windows-only.
- Draft and prerelease tags are ignored by update checks.
- Verification or staging failures stop the installation.

# Crust v0.4.5 Release Notes

Date: 2026-04-08

v0.4.5 adds Debian Linux support to the built-in auto-updater flow.

This release extends updater selection and install logic beyond Windows so Debian-style package artifacts can be discovered, verified, and installed from GitHub Releases.

## Highlights

- Debian Linux updater support for update checks and install scheduling
- Debian asset selection based on release artifact naming and system architecture
- SHA256 verification for Debian update artifacts before install
- New Debian release build script for producing updater-compatible .deb packages
- Workspace and internal crate versions bumped to 0.4.5

## Debian Updater Support

On Debian-like Linux systems, Crust now:

1. Queries the latest stable GitHub release.
2. Compares the latest release version against the running version.
3. Selects a Debian artifact matching the current architecture using this naming convention:
   - `crust-v<version>-debian-<arch>.deb`
4. Verifies the downloaded package digest using the release asset `sha256:<hex>` metadata.
5. Stages the package and launches the system installer via desktop openers.

## Release Packaging Requirements

To remain compatible with automatic update installs:

- Windows builds should continue to include a Windows x64 zip asset.
- Debian Linux builds should include a Debian package asset named:
  - `crust-v<version>-debian-<arch>.deb`
- Updater-consumed assets must expose a valid SHA256 digest in release metadata.

## New Debian Build Script

The repository now includes a Debian packaging helper script:

- `scripts/build_debian_release.sh`

It builds the release binary, creates a Debian package artifact, and writes a matching SHA256 file.

## Notes

- Auto-update install is now supported on Windows and Debian Linux systems.
- Stable-only checks remain in effect (draft/prerelease ignored).
- Unsupported Linux distributions continue to fail safely for updater operations.

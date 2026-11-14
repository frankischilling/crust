# Crust v0.4.2 release notes

Date: 2026-04-06

v0.4.2 fixes Windows updates that failed because the installer could not find
PowerShell.

## Highlights

- Fixed Windows updater installer launch reliability
- Added an absolute PowerShell path and fallback executable names
- Improved error reporting when installer launch cannot start

## Installer launch fix

Some Windows environments reported:

- failed to launch updater installer process: program not found

Root cause:

- The updater attempted to launch PowerShell using only a PATH-based command invocation.
- On systems where that command was not discoverable in the process environment, install could not start.

Fix in v0.4.2:

1. The updater now tries the absolute Windows PowerShell path first:
   - SystemRoot\\System32\\WindowsPowerShell\\v1.0\\powershell.exe
2. It then falls back through additional candidates:
   - powershell.exe
   - powershell
   - pwsh.exe
   - pwsh
3. If all launch attempts fail, the error now includes the full candidate list that was tried.

## Other updater behavior

- Update detection behavior is unchanged.
- SHA256 verification and staging flow are unchanged.
- Install and restart flow is unchanged except for more reliable installer process launch.

## Notes

- Auto-install remains Windows-only.
- Stable-only release checks remain in effect (draft/prerelease ignored).
- Release artifacts still require a Windows x64 zip asset with a valid SHA256 digest.

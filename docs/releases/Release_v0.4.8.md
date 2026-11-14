# Crust v0.4.8 release notes

Date: 2026-04-09

v0.4.8 refreshes Twitch badges from live services, handles EventSub WebSocket
control frames, and adds Linux desktop metadata.

## Highlights

- Global Twitch badge loading now refreshes from live APIs:
  1. Helix global badges (`/helix/chat/badges/global`) when OAuth is available
  2. IVR global badges fallback
  3. Legacy `badges.twitch.tv` fallback as a last step
- EventSub WebSocket handling:
  - Keeps the websocket writer alive for the full session
  - Responds to ping frames with pong frames
  - Logs close frames with server close code/reason when available
- 7TV emote rendering now prefers larger assets by default (4x, then 3x/2x/1x) so in-chat emotes appear sharper.
- Chat input placeholder text is clearer for account state, including phrasing around sending messages as the signed-in user.
- Debian packaging now installs desktop metadata:
  - `crust.desktop`
  - `crust.svg` icon
- App window icon updated to a purple `C` style for consistency with desktop branding.
- Workspace and internal crate versions bumped to 0.4.8.

## Upgrade notes

- No migration is required for settings or cache files.
- Badge coverage should improve automatically after startup as live global refreshes complete.

## Notes

- If global badge refresh endpoints are unavailable in an environment, bundled/global fallbacks still apply.
- EventSub still reconnects after a disconnect. Keeping the writer alive and
  responding to ping frames prevents avoidable connection drops.

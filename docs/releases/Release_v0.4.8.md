# Crust v0.4.8 Release Notes

Date: 2026-04-09

v0.4.8 focuses on Twitch metadata freshness, EventSub websocket stability, and Linux polish.

## Highlights

- Global Twitch badge loading now refreshes from live APIs:
  1. Helix global badges (`/helix/chat/badges/global`) when OAuth is available
  2. IVR global badges fallback
  3. Legacy `badges.twitch.tv` fallback as a last step
- EventSub websocket lifecycle hardened:
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

## Upgrade Notes

- No migration is required for settings or cache files.
- Badge coverage should improve automatically after startup as live global refreshes complete.

## Notes

- If global badge refresh endpoints are unavailable in an environment, bundled/global fallbacks still apply.
- EventSub reconnect behavior remains in place; this release reduces avoidable disconnect churn by handling websocket control frames correctly.

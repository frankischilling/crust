# Loader Progress Demo Plugin

This example focuses on low-level loader notifications and related image/emote events.

It shows:

- `c2.EventType.ImagePrefetchQueued`
- `c2.EventType.EmoteImageReady`
- `c2.EventType.EmoteCatalogUpdated`
- basic Lua-side counters for queued vs completed image work

Try:

- `/loadwatch`
- `/loadwatch reset`
- `/loadwatch fetch https://static-cdn.jtvnw.net/emoticons/v2/25/default/light/3.0`

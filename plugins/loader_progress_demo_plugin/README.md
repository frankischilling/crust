# Loader progress demo

Tracks queued and completed image requests through these events:

- `c2.EventType.ImagePrefetchQueued`
- `c2.EventType.EmoteImageReady`
- `c2.EventType.EmoteCatalogUpdated`

Use `/loadwatch` to inspect the counters, reset them, or request an image:

- `/loadwatch`
- `/loadwatch reset`
- `/loadwatch fetch https://static-cdn.jtvnw.net/emoticons/v2/25/default/light/3.0`

use std::{
    collections::HashMap,
    io::Cursor,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::sync::broadcast;

use bytes::Bytes;
use directories::ProjectDirs;
use lru::LruCache;
use tracing::debug;

use crate::{EmoteError, EmoteInfo};

const IN_MEMORY_CAPACITY: usize = 256;
const DISK_TTL: Duration = Duration::from_secs(24 * 3600); // 24 h for global

// AssetKey: identifies emote assets by provider, id, and scale

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AssetKey {
    pub provider: String,
    pub id: String,
    pub scale: u8, // 1, 2, or 4
}

// EmoteCache: manages in-memory and disk caching for emotes

/// Stores:
/// 1. An LRU in-memory byte cache (raw image bytes ready to decode).
/// 2. A disk cache under `~/.cache/crust/emotes/`.
/// 3. A code->EmoteInfo index (populated by loaders).
#[derive(Clone)]
pub struct EmoteCache {
    inner: Arc<Mutex<CacheInner>>,
    disk_dir: PathBuf,
    client: reqwest::Client,
}

struct CacheInner {
    bytes: LruCache<AssetKey, (Bytes, Instant)>,
    index: HashMap<String, EmoteInfo>, // code -> info
    /// In-flight request coalescing: URL -> broadcast sender.
    /// While a network fetch is in progress for a URL, subsequent callers
    /// subscribe to the same broadcast instead of issuing a duplicate request.
    in_flight: HashMap<String, broadcast::Sender<Result<(u32, u32, Vec<u8>), String>>>,
}

impl EmoteCache {
    pub fn new() -> Result<Self, EmoteError> {
        let dirs = ProjectDirs::from("dev", "crust", "crust")
            .ok_or_else(|| EmoteError::Io(std::io::Error::other("no config dir")))?;
        let disk_dir = dirs.cache_dir().join("emotes");
        std::fs::create_dir_all(&disk_dir)?;

        Ok(Self {
            inner: Arc::new(Mutex::new(CacheInner {
                bytes: LruCache::new(std::num::NonZeroUsize::new(IN_MEMORY_CAPACITY).unwrap()),
                index: HashMap::new(),
                in_flight: HashMap::new(),
            })),
            disk_dir,
            client: reqwest::Client::new(),
        })
    }

    /// Register a batch of emotes from a provider response.
    pub fn register(&self, emotes: Vec<EmoteInfo>) {
        let mut guard = self.inner.lock().unwrap();
        for e in emotes {
            guard.index.insert(e.code.clone(), e);
        }
    }

    /// Look up an emote by code.
    pub fn resolve(&self, code: &str) -> Option<EmoteInfo> {
        self.inner.lock().unwrap().index.get(code).cloned()
    }

    /// Fetch image bytes for the given key, using disk/memory caches.
    pub async fn get_bytes(&self, key: &AssetKey, url: &str) -> Result<Bytes, EmoteError> {
        // 1. Memory
        {
            let mut guard = self.inner.lock().unwrap();
            if let Some((bytes, ts)) = guard.bytes.get(key) {
                if ts.elapsed() < DISK_TTL {
                    return Ok(bytes.clone());
                }
            }
        }

        // 2. Disk
        let disk_path = self
            .disk_dir
            .join(&key.provider)
            .join(format!("{}_{}.bin", key.id, key.scale));

        if disk_path.exists() {
            if let Ok(data) = tokio::fs::read(&disk_path).await {
                let bytes: Bytes = data.into();
                let mut guard = self.inner.lock().unwrap();
                guard
                    .bytes
                    .put(key.clone(), (bytes.clone(), Instant::now()));
                return Ok(bytes);
            }
        }

        // 3. Network
        debug!("Fetching emote from {url}");
        let resp = self.client.get(url).send().await?;
        let bytes: Bytes = resp.bytes().await?;

        // Persist to disk
        if let Some(parent) = disk_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let _ = tokio::fs::write(&disk_path, &bytes).await;

        let mut guard = self.inner.lock().unwrap();
        guard
            .bytes
            .put(key.clone(), (bytes.clone(), Instant::now()));

        Ok(bytes)
    }

    pub fn emote_count(&self) -> usize {
        self.inner.lock().unwrap().index.len()
    }

    /// Fetch image bytes for a URL; return `(width, height, raw_bytes)`.
    /// Image dimensions are read from the file header only - no full RGBA decode.
    /// egui's built-in loaders (WebP / GIF / Image) handle decoding + animation.
    ///
    /// Checks memory -> disk -> network in order, writing through on a cache miss.
    pub async fn fetch_and_decode(&self, url: &str) -> Result<(u32, u32, Vec<u8>), EmoteError> {
        // Derive a stable disk path from a hash of the URL.
        let disk_path = self
            .disk_dir
            .join("url")
            .join(format!("{:016x}.bin", url_hash(url)));

        // 1. Memory cache (keyed by a synthetic AssetKey derived from the URL hash).
        let mem_key = AssetKey {
            provider: "url".into(),
            id: format!("{:016x}", url_hash(url)),
            scale: 1,
        };
        {
            let mut guard = self.inner.lock().unwrap();
            if let Some((bytes, ts)) = guard.bytes.get(&mem_key) {
                if ts.elapsed() < DISK_TTL {
                    let raw: Vec<u8> = bytes.to_vec();
                    let (w, h) = read_header_dims(&raw);
                    return Ok((w, h, raw));
                }
            }
        }

        // 2. Disk cache.
        if disk_path.exists() {
            if let Ok(data) = tokio::fs::read(&disk_path).await {
                let (w, h) = read_header_dims(&data);
                let bytes: Bytes = data.clone().into();
                self.inner
                    .lock()
                    .unwrap()
                    .bytes
                    .put(mem_key, (bytes, Instant::now()));
                return Ok((w, h, data));
            }
        }

        // 3. Network - with in-flight request coalescing.
        //    If another task is already fetching this URL, subscribe to its
        //    broadcast instead of issuing a duplicate request.
        let url_key = url.to_owned();
        let maybe_rx = {
            let mut guard = self.inner.lock().unwrap();
            if let Some(tx) = guard.in_flight.get(&url_key) {
                Some(tx.subscribe())
            } else {
                // Register ourselves as the in-flight fetcher.
                let (tx, _) = broadcast::channel(1);
                guard.in_flight.insert(url_key.clone(), tx);
                None
            }
            // guard dropped here - before any .await
        };

        if let Some(mut rx) = maybe_rx {
            // Another task is already fetching this URL - just wait for its result.
            return match rx.recv().await {
                Ok(Ok(result)) => Ok(result),
                Ok(Err(e)) => Err(EmoteError::Io(std::io::Error::other(e))),
                Err(_) => Err(EmoteError::Io(std::io::Error::other(
                    "in-flight fetch dropped",
                ))),
            };
        }

        debug!("Fetching emote image: {url}");
        let fetch_result: Result<(u32, u32, Vec<u8>), EmoteError> = async {
            let resp = self.client.get(url).send().await?;
            let raw_bytes = resp.bytes().await?.to_vec();

            // Write through to disk.
            if let Some(parent) = disk_path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            let _ = tokio::fs::write(&disk_path, &raw_bytes).await;

            {
                let bytes: Bytes = raw_bytes.clone().into();
                self.inner
                    .lock()
                    .unwrap()
                    .bytes
                    .put(mem_key, (bytes, Instant::now()));
            }

            let (w, h) = read_header_dims(&raw_bytes);
            Ok((w, h, raw_bytes))
        }
        .await;

        // Notify waiters and remove in-flight entry.
        {
            let mut guard = self.inner.lock().unwrap();
            if let Some(tx) = guard.in_flight.remove(&url_key) {
                let broadcast_val = match &fetch_result {
                    Ok(v) => Ok(v.clone()),
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(broadcast_val);
            }
        }

        fetch_result
    }

    /// Look up an emote by code - returns `(id, code, url, provider)` tuple
    /// suitable for the tokenizer callback.
    pub fn lookup_for_tokenizer(&self, code: &str) -> Option<(String, String, String, String)> {
        let guard = self.inner.lock().unwrap();
        guard.index.get(code).map(|info| {
            (
                info.id.clone(),
                info.code.clone(),
                info.url_1x.clone(),
                info.provider.clone(),
            )
        })
    }
}

// Helpers: utility functions for hashing and image dimension reading

/// Stable 64-bit hash of a URL string (FNV-1a).
fn url_hash(url: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in url.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Read image dimensions from the file header without full RGBA decoding.
/// Falls back to (1, 1) for unrecognized formats.
fn read_header_dims(bytes: &[u8]) -> (u32, u32) {
    image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
        .unwrap_or((1, 1))
}

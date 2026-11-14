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

use crate::{read_image_response, EmoteError, EmoteInfo};

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

/// Releases waiters even when the task owning a request is cancelled.
struct InFlightRequest {
    inner: Arc<Mutex<CacheInner>>,
    url: Option<String>,
}

impl InFlightRequest {
    fn finish(mut self, result: &Result<(u32, u32, Vec<u8>), EmoteError>) {
        let url = self.url.take().expect("active request");
        let sender = self.inner.lock().unwrap().in_flight.remove(&url);
        if let Some(sender) = sender.filter(|sender| sender.receiver_count() != 0) {
            let value = match result {
                Ok(value) => Ok(value.clone()),
                Err(error) => Err(error.to_string()),
            };
            let _ = sender.send(value);
        }
    }
}

impl Drop for InFlightRequest {
    fn drop(&mut self) {
        if let Some(url) = self.url.take() {
            self.inner.lock().unwrap().in_flight.remove(&url);
        }
    }
}

impl EmoteCache {
    pub fn new() -> Result<Self, EmoteError> {
        let dirs = ProjectDirs::from("dev", "crust", "crust")
            .ok_or_else(|| EmoteError::Io(std::io::Error::other("no config dir")))?;
        let disk_dir = dirs.cache_dir().join("emotes");
        Self::with_disk_dir(disk_dir)
    }

    fn with_disk_dir(disk_dir: PathBuf) -> Result<Self, EmoteError> {
        std::fs::create_dir_all(&disk_dir)?;

        Ok(Self {
            inner: Arc::new(Mutex::new(CacheInner {
                bytes: LruCache::new(std::num::NonZeroUsize::new(IN_MEMORY_CAPACITY).unwrap()),
                index: HashMap::new(),
                in_flight: HashMap::new(),
            })),
            disk_dir,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()?,
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

        if let Some((bytes, fetched_at)) = read_disk_cache(&disk_path).await {
            self.inner
                .lock()
                .unwrap()
                .bytes
                .put(key.clone(), (bytes.clone(), fetched_at));
            return Ok(bytes);
        }

        // 3. Network
        debug!("Fetching emote from {url}");
        let resp = self.client.get(url).send().await?;
        let bytes: Bytes = read_image_response(resp).await?.into();
        validate_image(bytes.clone()).await?;

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
    /// Cache misses are validated before storage. Warm memory hits only read
    /// dimensions; egui's loaders handle rendering and animation.
    ///
    /// Checks memory -> disk -> network in order, writing through on a cache miss.
    pub async fn fetch_and_decode(&self, url: &str) -> Result<(u32, u32, Vec<u8>), EmoteError> {
        // Derive a stable disk path from a hash of the URL.
        let url_id = format!("{:016x}", url_hash(url));
        let disk_path = self.disk_dir.join("url").join(format!("{url_id}.bin"));

        // 1. Memory cache (keyed by a synthetic AssetKey derived from the URL hash).
        let mem_key = AssetKey {
            provider: "url".into(),
            id: url_id,
            scale: 1,
        };
        let cached = {
            let mut guard = self.inner.lock().unwrap();
            guard
                .bytes
                .get(&mem_key)
                .filter(|(_, ts)| ts.elapsed() < DISK_TTL)
                .map(|(bytes, _)| bytes.clone())
        };
        if let Some(bytes) = cached {
            let (w, h) = read_header_dims(&bytes)?;
            return Ok((w, h, bytes.to_vec()));
        }

        // Coalesce both disk reads and network requests for the same URL.
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
        let request = InFlightRequest {
            inner: self.inner.clone(),
            url: Some(url_key),
        };

        let fetch_result: Result<(u32, u32, Vec<u8>), EmoteError> = async {
            let (bytes, fetched_at) = if let Some(cached) = read_disk_cache(&disk_path).await {
                cached
            } else {
                debug!("Fetching emote image: {url}");
                let resp = self.client.get(url).send().await?;
                let bytes: Bytes = read_image_response(resp).await?.into();
                validate_image(bytes.clone()).await?;
                if let Some(parent) = disk_path.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }
                let _ = tokio::fs::write(&disk_path, &bytes).await;
                (bytes, Instant::now())
            };
            let (w, h) = read_header_dims(&bytes)?;
            self.inner
                .lock()
                .unwrap()
                .bytes
                .put(mem_key, (bytes.clone(), fetched_at));
            Ok((w, h, bytes.to_vec()))
        }
        .await;

        request.finish(&fetch_result);
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

async fn read_disk_cache(path: &std::path::Path) -> Option<(Bytes, Instant)> {
    let modified = tokio::fs::metadata(path).await.ok()?.modified().ok()?;
    let age = modified.elapsed().ok()?;
    if age >= DISK_TTL {
        return None;
    }
    let bytes: Bytes = tokio::fs::read(path).await.ok()?.into();
    if validate_image(bytes.clone()).await.is_err() {
        let _ = tokio::fs::remove_file(path).await;
        return None;
    }
    Some((bytes, Instant::now().checked_sub(age)?))
}

async fn validate_image(bytes: Bytes) -> Result<(), EmoteError> {
    // Validate cache misses away from the async event loop. A readable header
    // alone does not prove that a response contains decodable image data.
    tokio::task::spawn_blocking(move || {
        image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()?
            .decode()?;
        Ok(())
    })
    .await
    .map_err(|error| EmoteError::Io(std::io::Error::other(error.to_string())))?
}

/// Read image dimensions without decoding the image into pixels.
fn read_header_dims(bytes: &[u8]) -> Result<(u32, u32), EmoteError> {
    Ok(image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .into_dimensions()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    struct TestCache(EmoteCache);

    impl TestCache {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let stamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "crust-emote-cache-{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            Self(EmoteCache::with_disk_dir(path).unwrap())
        }

        fn url_path(&self, url: &str) -> PathBuf {
            self.0
                .disk_dir
                .join("url")
                .join(format!("{:016x}.bin", url_hash(url)))
        }
    }

    impl Drop for TestCache {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0.disk_dir);
        }
    }

    fn png(width: u32) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(width, 1)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    async fn respond(stream: &mut TcpStream, status: &str, bytes: &[u8]) {
        respond_with_length(stream, status, bytes, bytes.len()).await;
    }

    async fn respond_with_length(stream: &mut TcpStream, status: &str, bytes: &[u8], length: usize) {
        let mut request = Vec::new();
        while !request.windows(4).any(|part| part == b"\r\n\r\n") {
            let mut chunk = [0; 1024];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0, "connection closed before request headers");
            request.extend_from_slice(&chunk[..count]);
            assert!(request.len() <= 16 * 1024, "request headers too large");
        }
        let headers = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(headers.as_bytes()).await.unwrap();
        stream.write_all(bytes).await.unwrap();
        stream.shutdown().await.unwrap();
    }

    async fn server(
        responses: Vec<(&'static str, Vec<u8>)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/emote", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            for (status, bytes) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                respond(&mut stream, status, &bytes).await;
            }
        });
        (url, task)
    }

    #[tokio::test]
    async fn failed_http_response_is_not_cached_and_can_retry() {
        let cache = TestCache::new();
        let bytes = png(2);
        let (url, task) = server(vec![
            ("503 Service Unavailable", bytes.clone()),
            ("200 OK", bytes.clone()),
        ])
        .await;
        assert!(matches!(
            cache.0.fetch_and_decode(&url).await,
            Err(EmoteError::Http(_))
        ));
        assert!(!cache.url_path(&url).exists());
        assert!(cache.0.inner.lock().unwrap().in_flight.is_empty());
        assert_eq!(cache.0.fetch_and_decode(&url).await.unwrap(), (2, 1, bytes));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_image_releases_coalesced_waiters_and_can_retry() {
        let cache = TestCache::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/emote", listener.local_addr().unwrap());
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            accepted_tx.send(()).unwrap();
            release_rx.await.unwrap();
            respond_with_length(&mut first, "200 OK", &[], crate::MAX_IMAGE_DOWNLOAD_BYTES + 1).await;
            let (mut retry, _) = listener.accept().await.unwrap();
            respond(&mut retry, "200 OK", &png(2)).await;
        });
        let leader_cache = cache.0.clone();
        let leader_url = url.clone();
        let leader = tokio::spawn(async move { leader_cache.fetch_and_decode(&leader_url).await });
        tokio::time::timeout(Duration::from_secs(3), accepted_rx).await.unwrap().unwrap();
        let waiter_cache = cache.0.clone();
        let waiter_url = url.clone();
        let waiter = tokio::spawn(async move { waiter_cache.fetch_and_decode(&waiter_url).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if cache.0.inner.lock().unwrap().in_flight.get(&url)
                    .is_some_and(|tx| tx.receiver_count() > 0)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        release_tx.send(()).unwrap();
        assert!(matches!(leader.await.unwrap(), Err(EmoteError::ImageTooLarge { .. })));
        assert!(waiter.await.unwrap().is_err());
        assert!(!cache.url_path(&url).exists());
        assert!(cache.0.inner.lock().unwrap().bytes.is_empty());
        assert!(cache.0.inner.lock().unwrap().in_flight.is_empty());
        assert_eq!(cache.0.fetch_and_decode(&url).await.unwrap(), (2, 1, png(2)));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn asset_key_fetch_rejects_oversized_response_before_cache_write() {
        let cache = TestCache::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/emote", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            respond_with_length(&mut stream, "200 OK", &[], crate::MAX_IMAGE_DOWNLOAD_BYTES + 1).await;
        });
        let key = AssetKey { provider: "test".into(), id: "large".into(), scale: 1 };
        assert!(matches!(cache.0.get_bytes(&key, &url).await, Err(EmoteError::ImageTooLarge { .. })));
        assert!(cache.0.inner.lock().unwrap().bytes.is_empty());
        assert!(!cache.0.disk_dir.join("test/large_1.bin").exists());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn image_header_without_pixel_data_does_not_poison_the_cache() {
        let cache = TestCache::new();
        let mut truncated = png(1);
        let image_data = truncated
            .windows(4)
            .position(|bytes| bytes == b"IDAT")
            .unwrap();
        truncated.truncate(image_data + 4);
        assert!(read_header_dims(&truncated).is_ok());
        assert!(image::load_from_memory(&truncated).is_err());
        let valid = png(1);
        let (url, task) = server(vec![("200 OK", truncated), ("200 OK", valid.clone())]).await;
        assert!(cache.0.fetch_and_decode(&url).await.is_err());
        assert!(!cache.url_path(&url).exists());
        assert_eq!(cache.0.fetch_and_decode(&url).await.unwrap(), (1, 1, valid));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn corrupt_and_expired_disk_entries_are_refetched() {
        for corrupt in [true, false] {
            let cache = TestCache::new();
            let bytes = png(2);
            let (url, task) = server(vec![("200 OK", bytes.clone())]).await;
            let path = cache.url_path(&url);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                if corrupt {
                    b"not an image".to_vec()
                } else {
                    png(1)
                },
            )
            .unwrap();
            if !corrupt {
                std::fs::File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(SystemTime::now() - DISK_TTL - Duration::from_secs(1))
                    .unwrap();
            }
            assert_eq!(cache.0.fetch_and_decode(&url).await.unwrap(), (2, 1, bytes));
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn disk_age_is_preserved_in_memory_and_requests_are_coalesced() {
        let cache = TestCache::new();
        let bytes = png(3);
        let (url, task) = server(vec![("200 OK", bytes.clone())]).await;
        let (first, second) = tokio::join!(
            cache.0.fetch_and_decode(&url),
            cache.0.fetch_and_decode(&url)
        );
        assert_eq!(first.unwrap(), (3, 1, bytes.clone()));
        assert_eq!(second.unwrap(), (3, 1, bytes.clone()));
        task.await.unwrap();

        cache.0.inner.lock().unwrap().bytes.clear();
        let age = Duration::from_secs(23 * 3600);
        std::fs::File::options()
            .write(true)
            .open(cache.url_path(&url))
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
        assert_eq!(cache.0.fetch_and_decode(&url).await.unwrap(), (3, 1, bytes));
        let inner = cache.0.inner.lock().unwrap();
        assert!(inner
            .bytes
            .iter()
            .all(|(_, (_, fetched_at))| fetched_at.elapsed() >= age));
    }

    #[tokio::test]
    async fn cancelled_fetch_releases_waiters_and_allows_a_new_request() {
        let cache = TestCache::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/emote", listener.local_addr().unwrap());
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (first, _) = listener.accept().await.unwrap();
            accepted_tx.send(()).unwrap();
            let (mut retry, _) = listener.accept().await.unwrap();
            respond(&mut retry, "200 OK", &png(2)).await;
            drop(first);
        });
        let leader_cache = cache.0.clone();
        let leader_url = url.clone();
        let leader = tokio::spawn(async move { leader_cache.fetch_and_decode(&leader_url).await });
        tokio::time::timeout(Duration::from_secs(3), accepted_rx)
            .await
            .unwrap()
            .unwrap();
        let waiter_cache = cache.0.clone();
        let waiter_url = url.clone();
        let waiter = tokio::spawn(async move { waiter_cache.fetch_and_decode(&waiter_url).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if cache
                    .0
                    .inner
                    .lock()
                    .unwrap()
                    .in_flight
                    .get(&url)
                    .is_some_and(|tx| tx.receiver_count() > 0)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        leader.abort();
        assert!(leader.await.unwrap_err().is_cancelled());
        assert!(tokio::time::timeout(Duration::from_secs(3), waiter)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert!(cache.0.inner.lock().unwrap().in_flight.is_empty());
        assert_eq!(cache.0.fetch_and_decode(&url).await.unwrap().0, 2);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn asset_key_fetch_rejects_error_responses_and_invalid_images() {
        let cache = TestCache::new();
        let key = AssetKey {
            provider: "test".to_owned(),
            id: "one".to_owned(),
            scale: 1,
        };
        let bytes = png(1);
        let (url, task) = server(vec![
            ("404 Not Found", bytes.clone()),
            ("200 OK", b"error page".to_vec()),
            ("200 OK", bytes.clone()),
        ])
        .await;
        assert!(cache.0.get_bytes(&key, &url).await.is_err());
        assert!(cache.0.get_bytes(&key, &url).await.is_err());
        assert_eq!(
            cache.0.get_bytes(&key, &url).await.unwrap().as_ref(),
            bytes.as_slice()
        );
        task.await.unwrap();
    }
}

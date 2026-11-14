use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crust_core::events::AppEvent;
use crust_emotes::{cache::EmoteCache, EmoteError};
use tokio::sync::{mpsc, Semaphore};
use tracing::debug;

type ImageResult = Result<(u32, u32, Vec<u8>), EmoteError>;

const MAX_IMAGE_DOWNLOADS: usize = 20;
const IMAGE_RETRY_POLICY: RetryPolicy = RetryPolicy {
    attempts: 3,
    initial_delay: Duration::from_secs(1),
    failure_cooldown: Duration::from_secs(30),
};

#[derive(Clone, Copy)]
struct RetryPolicy {
    attempts: usize,
    initial_delay: Duration,
    failure_cooldown: Duration,
}

#[derive(Clone, Copy)]
enum ImageRequestState {
    InFlight,
    Ready,
    RetryAfter(Instant),
}

/// Deduplicates reducer requests while allowing failed downloads to recover.
pub(crate) struct ImageRequests {
    states: Arc<Mutex<HashMap<String, ImageRequestState>>>,
    policy: RetryPolicy,
    slots: Arc<Semaphore>,
}

impl Default for ImageRequests {
    fn default() -> Self {
        Self {
            states: Arc::default(),
            policy: IMAGE_RETRY_POLICY,
            slots: image_download_slots().clone(),
        }
    }
}

impl ImageRequests {
    /// Starts at most one task per URL, including its automatic retries.
    pub(crate) fn request(
        &self,
        url: &str,
        cache: &Option<EmoteCache>,
        evt_tx: &mpsc::Sender<AppEvent>,
    ) -> bool {
        if evt_tx.is_closed() || url.trim().is_empty() {
            return false;
        }
        let Some(request) = self.begin(url, Instant::now()) else {
            return false;
        };
        let url = url.to_owned();
        let cache = cache.clone();
        let evt_tx = evt_tx.clone();
        let policy = self.policy;
        let slots = self.slots.clone();
        tokio::spawn(async move {
            // Keep one delayed request for callers that only ask once during cooldown.
            tokio::time::sleep_until(tokio::time::Instant::from_std(request.not_before)).await;
            let result = fetch_image_with_retry(&url, &cache, policy, &slots).await;
            if result.is_err() {
                // A completion can make a one-shot UI caller retry immediately.
                // Publish the cooldown first so that retry is queued, not lost.
                request.finish(false, Instant::now());
                send_image_result(&url, result, &evt_tx).await;
            } else {
                let delivered = send_image_result(&url, result, &evt_tx).await;
                request.finish(delivered, Instant::now());
            }
        });
        true
    }

    fn begin(&self, url: &str, now: Instant) -> Option<ImageRequest> {
        let mut states = self.states.lock().unwrap();
        let not_before = match states.get(url) {
            Some(ImageRequestState::InFlight | ImageRequestState::Ready) => return None,
            Some(ImageRequestState::RetryAfter(retry_at)) => (*retry_at).max(now),
            None => now,
        };
        states.insert(url.to_owned(), ImageRequestState::InFlight);
        Some(ImageRequest {
            states: self.states.clone(),
            url: Some(url.to_owned()),
            failure_cooldown: self.policy.failure_cooldown,
            not_before,
        })
    }
}

struct ImageRequest {
    states: Arc<Mutex<HashMap<String, ImageRequestState>>>,
    url: Option<String>,
    failure_cooldown: Duration,
    not_before: Instant,
}

impl ImageRequest {
    fn finish(mut self, succeeded: bool, now: Instant) {
        self.settle(succeeded, now);
    }

    fn settle(&mut self, succeeded: bool, now: Instant) {
        if let Some(url) = self.url.take() {
            let state = if succeeded {
                ImageRequestState::Ready
            } else {
                ImageRequestState::RetryAfter(now + self.failure_cooldown)
            };
            self.states.lock().unwrap().insert(url, state);
        }
    }
}

impl Drop for ImageRequest {
    fn drop(&mut self) {
        // A cancelled or panicking fetch must not leave its URL in flight forever.
        self.settle(false, Instant::now());
    }
}

fn image_download_slots() -> &'static Arc<Semaphore> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(Semaphore::new(MAX_IMAGE_DOWNLOADS)))
}

/// Fetch one image and emit exactly one success or failure completion.
pub(crate) async fn fetch_emote_image(
    url: &str,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let result =
        fetch_image_with_retry(url, cache, IMAGE_RETRY_POLICY, image_download_slots()).await;
    send_image_result(url, result, evt_tx).await;
}

async fn fetch_image_with_retry(
    url: &str,
    cache: &Option<EmoteCache>,
    policy: RetryPolicy,
    slots: &Semaphore,
) -> ImageResult {
    let mut attempts = 0;
    let mut delay = policy.initial_delay;
    loop {
        let result = fetch_image_once(url, cache, slots).await;
        attempts += 1;
        if result.is_ok() || attempts >= policy.attempts {
            return result;
        }
        // fetch_image_once releases its download slot before the retry delay.
        tokio::time::sleep(delay).await;
        delay = delay.saturating_mul(2);
    }
}

async fn fetch_image_once(url: &str, cache: &Option<EmoteCache>, slots: &Semaphore) -> ImageResult {
    let _permit = slots
        .acquire()
        .await
        .map_err(|error| EmoteError::Io(std::io::Error::other(error.to_string())))?;
    if let Some(cache) = cache {
        cache.fetch_and_decode(url).await
    } else {
        fetch_and_decode_raw_unlimited(url).await
    }
}

async fn send_image_result(
    url: &str,
    result: ImageResult,
    evt_tx: &mpsc::Sender<AppEvent>,
) -> bool {
    let succeeded = result.is_ok();
    let (width, height, raw_bytes) = result.unwrap_or_else(|error| {
        debug!("Failed to fetch emote image {url}: {error}");
        // One empty completion settles the loading counter after all retries fail.
        (0, 0, Vec::new())
    });
    evt_tx
        .send(AppEvent::EmoteImageReady {
            uri: url.to_owned(),
            width,
            height,
            raw_bytes,
        })
        .await
        .is_ok()
        && succeeded
}

pub(crate) async fn fetch_and_decode_raw(url: &str) -> ImageResult {
    fetch_image_once(url, &None, image_download_slots()).await
}

async fn fetch_and_decode_raw_unlimited(url: &str) -> ImageResult {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if CLIENT.get().is_none() {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?;
        let _ = CLIENT.set(client);
    }
    let raw = CLIENT
        .get()
        .expect("image client initialized")
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?
        .to_vec();
    tokio::task::spawn_blocking(move || {
        let decoded = image::ImageReader::new(std::io::Cursor::new(&raw))
            .with_guessed_format()?
            .decode()?;
        Ok((decoded.width(), decoded.height(), raw))
    })
    .await
    .map_err(|error| EmoteError::Io(std::io::Error::other(error.to_string())))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn requests(attempts: usize) -> ImageRequests {
        ImageRequests {
            states: Arc::default(),
            policy: RetryPolicy {
                attempts,
                initial_delay: Duration::ZERO,
                failure_cooldown: Duration::from_secs(30),
            },
            slots: Arc::new(Semaphore::new(2)),
        }
    }

    fn png() -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2, 1)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    async fn server(responses: Vec<(u16, Vec<u8>)>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/image", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            for (status, bytes) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                    let mut chunk = [0; 1024];
                    let count = stream.read(&mut chunk).await.unwrap();
                    assert!(count > 0, "connection closed before request headers");
                    request.extend_from_slice(&chunk[..count]);
                    assert!(request.len() <= 16 * 1024, "request headers too large");
                }
                let headers = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(&bytes).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        (url, task)
    }

    #[tokio::test]
    async fn transient_and_corrupt_responses_retry_without_duplicate_completion() {
        let valid = png();
        let mut corrupt = valid.clone();
        let image_data = corrupt
            .windows(4)
            .position(|bytes| bytes == b"IDAT")
            .unwrap();
        corrupt.truncate(image_data + 4);
        let (url, server) = server(vec![
            (503, Vec::new()),
            (200, corrupt),
            (200, valid.clone()),
        ])
        .await;
        let requests = requests(3);
        let (tx, mut rx) = mpsc::channel(8);
        assert!(requests.request(&url, &None, &tx));
        assert!(!requests.request(&url, &None, &tx));
        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(event, AppEvent::EmoteImageReady { width: 2, height: 1, raw_bytes, .. } if raw_bytes == valid)
        );
        assert!(rx.try_recv().is_err());
        assert!(!requests.request(&url, &None, &tx));
        assert!(matches!(
            requests.states.lock().unwrap().get(&url),
            Some(ImageRequestState::Ready)
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn exhausted_retries_accept_one_delayed_request_during_cooldown() {
        let valid = png();
        let (url, server) = server(vec![
            (503, Vec::new()),
            (503, Vec::new()),
            (200, valid.clone()),
        ])
        .await;
        let mut requests = requests(2);
        requests.policy.failure_cooldown = Duration::from_millis(10);
        let (tx, mut rx) = mpsc::channel(8);
        assert!(requests.request(&url, &None, &tx));
        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(event, AppEvent::EmoteImageReady { width: 0, height: 0, raw_bytes, .. } if raw_bytes.is_empty())
        );
        assert!(rx.try_recv().is_err());
        assert!(requests.request(&url, &None, &tx));
        assert!(!requests.request(&url, &None, &tx));
        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, AppEvent::EmoteImageReady { raw_bytes, .. } if raw_bytes == valid));
        assert!(rx.try_recv().is_err());
        assert!(!requests.request(&url, &None, &tx));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn failure_state_is_released_before_a_blocked_completion_is_published() {
        let (url, server) = server(vec![(503, Vec::new())]).await;
        let requests = requests(1);
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(AppEvent::EmoteImageReady {
            uri: "occupied".to_owned(),
            width: 0,
            height: 0,
            raw_bytes: Vec::new(),
        })
        .await
        .unwrap();
        assert!(requests.request(&url, &None, &tx));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let released = matches!(
                    requests.states.lock().unwrap().get(&url),
                    Some(ImageRequestState::RetryAfter(_)),
                );
                if released {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a blocked completion must not keep the URL in flight");
        rx.recv().await.unwrap();
        let event = rx.recv().await.unwrap();
        assert!(
            matches!(event, AppEvent::EmoteImageReady { uri, raw_bytes, .. } if uri == url && raw_bytes.is_empty())
        );
        server.await.unwrap();
    }

    #[test]
    fn cooldown_delays_one_request_and_success_is_retained() {
        let requests = requests(3);
        let now = Instant::now();
        let request = requests.begin("https://example.com/image", now).unwrap();
        assert!(requests.begin("https://example.com/image", now).is_none());
        request.finish(false, now);
        let request = requests
            .begin("https://example.com/image", now + Duration::from_secs(29))
            .unwrap();
        assert_eq!(request.not_before, now + Duration::from_secs(30));
        assert!(requests
            .begin("https://example.com/image", now + Duration::from_secs(29))
            .is_none());
        request.finish(true, now + Duration::from_secs(30));
        assert!(requests
            .begin("https://example.com/image", now + Duration::from_secs(3600))
            .is_none());
    }

    #[test]
    fn cancelled_requests_release_the_url_after_backoff() {
        let requests = requests(3);
        let now = Instant::now();
        let request = requests.begin("https://example.com/image", now).unwrap();
        drop(request);
        let retry = requests
            .begin("https://example.com/image", Instant::now())
            .unwrap();
        assert!(retry.not_before >= now + Duration::from_secs(30));
        assert!(requests.begin("https://example.com/image", now).is_none());
    }

    #[tokio::test]
    async fn retry_delay_releases_the_shared_download_slot() {
        let (url, server) = server(vec![(503, Vec::new())]).await;
        let slots = Arc::new(Semaphore::new(1));
        let task_slots = slots.clone();
        let task = tokio::spawn(async move {
            fetch_image_with_retry(
                &url,
                &None,
                RetryPolicy {
                    attempts: 2,
                    initial_delay: Duration::from_secs(60),
                    failure_cooldown: Duration::ZERO,
                },
                &task_slots,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        let permit = tokio::time::timeout(Duration::from_secs(2), slots.acquire())
            .await
            .unwrap()
            .unwrap();
        assert!(!task.is_finished());
        drop(permit);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
}

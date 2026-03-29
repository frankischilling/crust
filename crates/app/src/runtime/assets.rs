use crust_core::events::AppEvent;
use crust_emotes::cache::EmoteCache;
use tokio::sync::mpsc;
use tracing::debug;

/// Fetch a single emote/emoji/badge image and send raw bytes to UI.
pub(crate) async fn fetch_emote_image(
    url: &str,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let result = if let Some(cache) = cache {
        cache.fetch_and_decode(url).await
    } else {
        fetch_and_decode_raw(url).await
    };

    match result {
        Ok((width, height, raw_bytes)) => {
            let _ = evt_tx
                .send(AppEvent::EmoteImageReady {
                    uri: url.to_owned(),
                    width,
                    height,
                    raw_bytes,
                })
                .await;
        }
        Err(e) => {
            debug!("Failed to fetch emote image {url}: {e}");
            // Emit a zero-byte stub so the loading screen can count this
            // fetch as settled (prevents hanging on failures).
            let _ = evt_tx
                .send(AppEvent::EmoteImageReady {
                    uri: url.to_owned(),
                    width: 0,
                    height: 0,
                    raw_bytes: vec![],
                })
                .await;
        }
    }
}

pub(crate) async fn fetch_and_decode_raw(
    url: &str,
) -> std::result::Result<(u32, u32, Vec<u8>), crust_emotes::EmoteError> {
    let client = reqwest::Client::new();
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        return Err(crust_emotes::EmoteError::NotFound(format!(
            "HTTP {} for {url}",
            resp.status()
        )));
    }
    let raw = resp.bytes().await?;
    let raw_vec = raw.to_vec();
    // Read dimensions from header only - no full RGBA decode needed
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(&raw_vec))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
        .unwrap_or((1, 1));
    Ok((w, h, raw_vec))
}

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crust_core::events::AppEvent;
use crust_emotes::cache::EmoteCache;
use tokio::sync::mpsc;

use crate::runtime::assets::fetch_emote_image;

const MAX_PENDING_LINK_PREVIEWS: usize = 64;
const MAX_PREVIEW_HTML_BYTES: usize = 64 * 1024;

/// Keep only active requests so a preview evicted by the UI can be fetched again.
#[derive(Default)]
pub(crate) struct LinkPreviewRequests {
    pending: Arc<Mutex<HashSet<String>>>,
}

impl LinkPreviewRequests {
    pub(crate) fn begin(&self, url: &str) -> Option<LinkPreviewRequest> {
        let mut pending = self.pending.lock().unwrap();
        if pending.len() >= MAX_PENDING_LINK_PREVIEWS || !pending.insert(url.to_owned()) {
            return None;
        }
        Some(LinkPreviewRequest {
            pending: self.pending.clone(),
            url: url.to_owned(),
        })
    }
}

pub(crate) struct LinkPreviewRequest {
    pending: Arc<Mutex<HashSet<String>>>,
    url: String,
}

impl Drop for LinkPreviewRequest {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.url);
    }
}

/// Link preview fetch.
pub(crate) async fn fetch_link_preview(
    url: &str,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let send_empty = |url: &str| AppEvent::LinkPreviewReady {
        url: url.to_owned(),
        title: None,
        description: None,
        thumbnail_url: None,
        site_name: None,
    };

    // Use a realistic Chrome User-Agent so sites like Twitter / YouTube
    // don't serve empty bot pages or block us entirely.
    let client = match reqwest::Client::builder()
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
             AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/131.0.0.0 Safari/537.36",
        )
        .timeout(std::time::Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            let _ = evt_tx.send(send_empty(url)).await;
            return;
        }
    };

    // Direct image URLs (image hosts, uploader output, hot-linked assets)
    // don't serve OG tagsthey serve the image itself. Short-circuit by
    // using the URL as its own thumbnail so the hover tooltip shows a real
    // image preview instead of "Loading preview..." forever.
    if is_direct_image_url(url) {
        fetch_emote_image(url, cache, evt_tx).await;
        let _ = evt_tx
            .send(AppEvent::LinkPreviewReady {
                url: url.to_owned(),
                title: None,
                description: None,
                thumbnail_url: Some(url.to_owned()),
                site_name: detect_site_name(url),
            })
            .await;
        return;
    }

    // YouTube serves proper OG tags but the oEmbed JSON API is faster,
    // more reliable, and doesn't require HTML parsing.
    if is_youtube_url(url) {
        if let Some(ev) = fetch_youtube_oembed(url, &client, cache, evt_tx).await {
            let _ = evt_tx.send(ev).await;
            return;
        }
        // Fall through to generic OG-tag path on failure.
    }

    // twitter.com and x.com serve JavaScript-rendered pages that bots
    // cannot parse. fxtwitter.com is a public proxy that serves proper
    // OG meta tags for tweet URLs.
    let fetch_url = rewrite_twitter_url(url).unwrap_or_else(|| url.to_owned());

    let resp = match client.get(&fetch_url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => {
            let _ = evt_tx.send(send_empty(url)).await;
            return;
        }
    };

    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    // Direct-image fallback: URLs without a known image extension but
    // served with an image content-type (CDN shorteners, redirects,
    // /raw/... endpoints). Use the URL itself as the thumbnail.
    if ct.starts_with("image/") {
        fetch_emote_image(url, cache, evt_tx).await;
        let _ = evt_tx
            .send(AppEvent::LinkPreviewReady {
                url: url.to_owned(),
                title: None,
                description: None,
                thumbnail_url: Some(url.to_owned()),
                site_name: detect_site_name(url),
            })
            .await;
        return;
    }
    if !ct.contains("html") {
        let _ = evt_tx.send(send_empty(url)).await;
        return;
    }

    let bytes = match read_preview_html_prefix(resp).await {
        Ok(b) => b,
        Err(_) => {
            let _ = evt_tx.send(send_empty(url)).await;
            return;
        }
    };
    let html = String::from_utf8_lossy(&bytes);

    let title = og_meta(&html, "og:title")
        .or_else(|| og_meta(&html, "twitter:title"))
        .or_else(|| html_title(&html));
    let description =
        og_meta(&html, "og:description").or_else(|| og_meta(&html, "twitter:description"));
    let thumbnail_url = og_meta(&html, "og:image").or_else(|| og_meta(&html, "twitter:image"));
    let site_name = og_meta(&html, "og:site_name").or_else(|| detect_site_name(url));

    // Kick off thumbnail image fetch so bytes land in emote_bytes.
    if let Some(ref img) = thumbnail_url {
        fetch_emote_image(img, cache, evt_tx).await;
    }

    let _ = evt_tx
        .send(AppEvent::LinkPreviewReady {
            url: url.to_owned(),
            title,
            description,
            thumbnail_url,
            site_name,
        })
        .await;
}

/// Metadata normally appears near the start of a page. Stop at the prefix limit
/// without waiting for the rest of a large or continuing response body.
async fn read_preview_html_prefix(mut response: reqwest::Response) -> Result<Vec<u8>, reqwest::Error> {
    let capacity = response.content_length().unwrap_or(0).min(MAX_PREVIEW_HTML_BYTES as u64);
    let mut bytes = Vec::with_capacity(capacity as usize);
    while bytes.len() < MAX_PREVIEW_HTML_BYTES {
        let Some(chunk) = response.chunk().await? else { break };
        let keep = chunk.len().min(MAX_PREVIEW_HTML_BYTES - bytes.len());
        bytes.extend_from_slice(&chunk[..keep]);
    }
    Ok(bytes)
}

/// Check if a URL is a YouTube / youtu.be link.
fn is_youtube_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.contains("youtube.com/") || lower.contains("youtu.be/")
}

/// Return true when the URL's path looks like a direct image asset.
/// Strips query + fragment so `?token=...` / `#anchor` don't break matching.
fn is_direct_image_url(url: &str) -> bool {
    let path = url
        .split('#')
        .next()
        .and_then(|p| p.split('?').next())
        .unwrap_or(url);
    let lower = path.to_ascii_lowercase();
    matches!(
        lower.rsplit('.').next(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "avif")
    )
}

#[cfg(test)]
mod tests {
    use super::{fetch_link_preview, is_direct_image_url, read_preview_html_prefix, LinkPreviewRequests,
        MAX_PENDING_LINK_PREVIEWS, MAX_PREVIEW_HTML_BYTES};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn continuing_preview_response(headers: String, body: Vec<u8>) -> (
        reqwest::Response,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/preview", listener.local_addr().unwrap());
        let (close, closed) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let mut chunk = [0; 1024];
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0, "request closed before its headers");
                request.extend_from_slice(&chunk[..count]);
                assert!(request.len() <= 16 * 1024);
            }
            stream.write_all(headers.as_bytes()).await.unwrap();
            // A bounded reader may close the socket before this oversized body is written.
            let _ = stream.write_all(&body).await;
            let _ = closed.await;
        });
        let response = reqwest::Client::builder().no_proxy()
            .timeout(Duration::from_secs(3)).build().unwrap()
            .get(url).send().await.unwrap();
        (response, close, server)
    }

    #[tokio::test]
    async fn link_preview_html_finishes_at_prefix_cap_without_waiting_for_body_end() {
        for chunked in [false, true] {
            let mut body = vec![b' '; MAX_PREVIEW_HTML_BYTES];
            let title = b"<title>Preview title</title>";
            body[..title.len()].copy_from_slice(title);
            body.extend_from_slice(b"<meta property=\"og:description\" content=\"outside prefix\">");
            let declared = MAX_PREVIEW_HTML_BYTES * 8;
            let framing = if chunked {
                let mut wire = format!("{declared:x}\r\n").into_bytes();
                wire.extend_from_slice(&body);
                body = wire;
                "Transfer-Encoding: chunked".to_owned()
            } else {
                format!("Content-Length: {declared}")
            };
            let (response, close, server) = continuing_preview_response(
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n{framing}\r\n\r\n"),
                body,
            ).await;
            // The server deliberately keeps an incomplete body open until after this result.
            let bytes = tokio::time::timeout(Duration::from_secs(2), read_preview_html_prefix(response))
                .await.expect("preview must not wait for the oversized body to end").unwrap();
            assert_eq!(bytes.len(), MAX_PREVIEW_HTML_BYTES);
            let html = String::from_utf8_lossy(&bytes);
            assert_eq!(super::html_title(&html).as_deref(), Some("Preview title"));
            assert_eq!(super::og_meta(&html, "og:description"), None);
            drop(close);
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn link_preview_html_preserves_errors_before_prefix_cap() {
        let (response, close, server) = continuing_preview_response(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 1024\r\n\r\n".into(),
            b"<title>Incomplete page</title>".to_vec(),
        ).await;
        drop(close);
        server.await.unwrap();
        assert!(read_preview_html_prefix(response).await.is_err());
    }

    #[tokio::test]
    async fn link_preview_requests_release_after_failed_fetch_publishes_result() {
        let requests = LinkPreviewRequests::default();
        let url = "not a valid URL";
        let request = requests.begin(url).unwrap();
        let (events, mut received) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(async move {
            let _request = request;
            fetch_link_preview(url, &None, &events).await;
        });
        assert!(requests.begin(url).is_none());
        let event = received.recv().await.expect("failed fetch publishes an empty result");
        assert!(matches!(event, crust_core::AppEvent::LinkPreviewReady {
            title: None, description: None, thumbnail_url: None, ..
        }));
        task.await.unwrap();
        assert!(requests.begin(url).is_some(), "failed requests must permit a later retry");
    }

    #[test]
    fn link_preview_requests_deduplicate_bound_and_release_for_refetch() {
        let requests = LinkPreviewRequests::default();
        let url = "https://example.test/preview";
        let first = requests.begin(url).unwrap();
        assert!(requests.begin(url).is_none());
        let mut others = Vec::new();
        for index in 1..MAX_PENDING_LINK_PREVIEWS {
            others.push(requests.begin(&format!("https://example.test/{index}")).unwrap());
        }
        assert!(requests.begin("https://example.test/overflow").is_none());
        drop(first);
        let refetch = requests.begin(url).expect("completed requests can be fetched again");
        assert!(requests.begin(url).is_none());
        drop(refetch);
        drop(others);
        assert!(requests.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn link_preview_requests_release_when_fetch_task_is_cancelled() {
        let requests = LinkPreviewRequests::default();
        let url = "https://example.test/cancelled";
        let request = requests.begin(url).unwrap();
        let task = tokio::spawn(async move {
            let _request = request;
            std::future::pending::<()>().await;
        });
        assert!(requests.begin(url).is_none());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(requests.begin(url).is_some());
    }

    #[test]
    fn direct_image_detection() {
        assert!(is_direct_image_url("https://i.nuuls.com/5RpNP.png"));
        assert!(is_direct_image_url("https://i.imgur.com/abc.JPG"));
        assert!(is_direct_image_url("https://example.com/a/b/c.webp?v=2"));
        assert!(is_direct_image_url("https://example.com/x.gif#t=5"));
        assert!(!is_direct_image_url("https://example.com/page.html"));
        assert!(!is_direct_image_url("https://youtube.com/watch?v=abc"));
    }
}

/// Fetch YouTube video metadata via the public oEmbed JSON endpoint.
/// Returns `Some(AppEvent)` on success, `None` on failure.
async fn fetch_youtube_oembed(
    original_url: &str,
    client: &reqwest::Client,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) -> Option<AppEvent> {
    let oembed_url = format!(
        "https://www.youtube.com/oembed?url={}&format=json",
        url_percent_encode(original_url)
    );
    let resp = client.get(&oembed_url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    let title = json["title"].as_str().map(|s| s.to_owned());
    let author = json["author_name"].as_str().map(|s| s.to_owned());
    let thumbnail_url = json["thumbnail_url"].as_str().map(|s| s.to_owned());

    // Fetch the thumbnail image bytes.
    if let Some(ref img) = thumbnail_url {
        fetch_emote_image(img, cache, evt_tx).await;
    }

    Some(AppEvent::LinkPreviewReady {
        url: original_url.to_owned(),
        title,
        description: author.map(|a| format!("by {a}")),
        thumbnail_url,
        site_name: Some("YouTube".to_owned()),
    })
}

/// Rewrite twitter.com / x.com URLs to fxtwitter.com so we get proper OG
/// meta tags instead of a JS-rendered blank page.
fn rewrite_twitter_url(url: &str) -> Option<String> {
    let lower = url.to_lowercase();
    // Match URLs like https://twitter.com/user/status/... or https://x.com/user/status/...
    if lower.contains("twitter.com/") || lower.contains("x.com/") {
        // Only rewrite status/tweet URLs, not profile pages.
        if lower.contains("/status/") {
            let rewritten = url
                .replace("twitter.com", "fxtwitter.com")
                .replace("x.com", "fxtwitter.com");
            return Some(rewritten);
        }
    }
    None
}

/// Heuristic site-name detection from the URL hostname when og:site_name
/// is missing from the HTML.
fn detect_site_name(url: &str) -> Option<String> {
    let lower = url.to_lowercase();
    if lower.contains("youtube.com/") || lower.contains("youtu.be/") {
        Some("YouTube".to_owned())
    } else if lower.contains("twitter.com/")
        || lower.contains("x.com/")
        || lower.contains("fxtwitter.com/")
    {
        Some("Twitter".to_owned())
    } else if lower.contains("twitch.tv/") {
        Some("Twitch".to_owned())
    } else if lower.contains("reddit.com/") {
        Some("Reddit".to_owned())
    } else if lower.contains("instagram.com/") {
        Some("Instagram".to_owned())
    } else if lower.contains("tiktok.com/") {
        Some("TikTok".to_owned())
    } else if lower.contains("github.com/") {
        Some("GitHub".to_owned())
    } else if lower.contains("wikipedia.org/") {
        Some("Wikipedia".to_owned())
    } else if lower.contains("steamcommunity.com/") || lower.contains("store.steampowered.com/") {
        Some("Steam".to_owned())
    } else if lower.contains("clips.twitch.tv/") {
        Some("Twitch Clip".to_owned())
    } else {
        None
    }
}

/// Extract the content of a `<meta property=\"{prop}\" ...>` or `<meta name=\"{prop}\" ...>` tag.
fn og_meta(html: &str, prop: &str) -> Option<String> {
    let prop_lower = prop.to_lowercase();
    let mut offset = 0;
    while let Some(rel) = html[offset..].to_lowercase().find("<meta") {
        let abs = offset + rel;
        let rest = &html[abs..];
        // Find end of this tag
        let tag_end = rest.find('>').unwrap_or(rest.len()).min(512);
        let tag = &rest[..tag_end];
        let tag_lower = tag.to_lowercase();

        let has_prop = tag_lower.contains(&format!("property=\"{prop_lower}\""))
            || tag_lower.contains(&format!("property='{prop_lower}'"))
            || tag_lower.contains(&format!("name=\"{prop_lower}\""))
            || tag_lower.contains(&format!("name='{prop_lower}'"));

        if has_prop {
            if let Some(val) = html_attr(tag, "content") {
                return Some(html_entities(val));
            }
        }
        offset = abs + 5;
    }
    None
}

/// Extract an attribute value from an HTML tag snippet.
fn html_attr<'a>(tag: &'a str, attr: &str) -> Option<&'a str> {
    let tag_lower = tag.to_lowercase();
    let needle = format!("{}=", attr.to_lowercase());
    let pos = tag_lower.find(&needle)?;
    let after = &tag[pos + needle.len()..];
    if after.starts_with('"') {
        let end = after[1..].find('"')?;
        Some(&after[1..1 + end])
    } else if after.starts_with('\'') {
        let end = after[1..].find('\'')?;
        Some(&after[1..1 + end])
    } else {
        None
    }
}

/// Extract `<title>` text as a fallback.
fn html_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let s = lower.find("<title")? + 6;
    let tag_end = lower[s..].find('>')?;
    let body_start = s + tag_end + 1;
    let body_end = lower[body_start..].find("</title>")?;
    let text = html[body_start..body_start + body_end].trim().to_owned();
    if text.is_empty() {
        None
    } else {
        Some(html_entities(&text))
    }
}

/// Decode common HTML entities.
fn html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

/// Minimal percent-encoding for query-string values (no external crate needed).
fn url_percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(char::from(HEX_CHARS[(b >> 4) as usize]));
                out.push(char::from(HEX_CHARS[(b & 0xf) as usize]));
            }
        }
    }
    out
}

const HEX_CHARS: [u8; 16] = *b"0123456789ABCDEF";

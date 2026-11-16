use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crust_core::events::AppEvent;
use tokio::sync::mpsc;

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

    // Direct image links use their URL as the thumbnail. The visible tooltip
    // requests its bytes through the same queue as other images.
    if is_direct_image_url(url) {
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
        if let Some(ev) = fetch_youtube_oembed(url, &client).await {
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

    let source_url = resp.url().clone();
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
        drop(resp);
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
    let thumbnail_url = html_thumbnail_url(&html, &source_url);
    let site_name = og_meta(&html, "og:site_name").or_else(|| detect_site_name(url));

    // Visible previews request thumbnails through the shared UI image queue.
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

fn web_url(url: &str) -> Option<reqwest::Url> {
    let parsed = reqwest::Url::parse(url).ok()?;
    (matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some())
        .then_some(parsed)
}

fn host_matches(host: &str, domain: &str) -> bool {
    let host = host.trim_end_matches('.');
    host == domain || host.strip_suffix(domain).is_some_and(|prefix| prefix.ends_with('.'))
}

fn resolve_thumbnail_url(base: &reqwest::Url, image: &str) -> Option<String> {
    let image = image.trim();
    if image.is_empty() {
        return None;
    }
    let resolved = base.join(image).ok()?;
    (matches!(resolved.scheme(), "http" | "https") && resolved.host_str().is_some())
        .then(|| resolved.to_string())
}

fn html_thumbnail_url(html: &str, base: &reqwest::Url) -> Option<String> {
    ["og:image", "twitter:image"].into_iter().find_map(|property| {
        og_meta(html, property).and_then(|image| resolve_thumbnail_url(base, &image))
    })
}

/// Check the parsed hostname for a YouTube video service.
fn is_youtube_url(url: &str) -> bool {
    let Some(url) = web_url(url) else { return false };
    let host = url.host_str().unwrap_or_default();
    host_matches(host, "youtube.com") || host_matches(host, "youtu.be")
}

/// Return true when the URL's path looks like a direct image asset.
/// Strips query + fragment so `?token=...` / `#anchor` don't break matching.
fn is_direct_image_url(url: &str) -> bool {
    let Some(url) = web_url(url) else { return false };
    let lower = url.path().to_ascii_lowercase();
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
            fetch_link_preview(url, &events).await;
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

    #[test]
    fn preview_hosts_use_domain_boundaries_instead_of_url_substrings() {
        for url in ["https://youtube.com", "https://M.YOUTUBE.COM/watch?v=123", "https://youtu.be/123"] {
            assert!(super::is_youtube_url(url), "{url}");
        }
        for url in [
            "https://notyoutube.com/watch?v=123",
            "https://youtube.com.example.test/watch?v=123",
            "https://example.test/youtube.com/watch?v=123",
            "https://example.test/?next=https://youtu.be/123",
            "https://youtube.com@example.test/watch?v=123",
        ] {
            assert!(!super::is_youtube_url(url), "{url}");
            assert_eq!(super::detect_site_name(url), None, "{url}");
        }
        assert_eq!(super::detect_site_name("https://clips.twitch.tv/Clip"), Some("Twitch Clip".into()));
        assert_eq!(super::detect_site_name("https://EN.WIKIPEDIA.ORG/wiki/Test"), Some("Wikipedia".into()));
        assert!(!is_direct_image_url("https://example.png"));
        assert!(!is_direct_image_url("https://example.test/?image=photo.png"));
    }

    #[test]
    fn preview_tweet_rewrites_change_only_a_supported_hostname() {
        assert_eq!(super::rewrite_twitter_url("https://X.COM/user/status/123?next=x.com#twitter.com"),
            Some("https://fxtwitter.com/user/status/123?next=x.com#twitter.com".into()));
        assert_eq!(super::rewrite_twitter_url("https://mobile.twitter.com/user/status/123"),
            Some("https://fxtwitter.com/user/status/123".into()));
        for url in [
            "https://fxtwitter.com/user/status/123",
            "https://notx.com/user/status/123",
            "https://twitter.com.example.test/user/status/123",
            "https://example.test/x.com/user/status/123",
            "https://example.test/?next=https://x.com/user/status/123",
            "https://x.com/user?next=/status/123",
        ] {
            assert_eq!(super::rewrite_twitter_url(url), None, "{url}");
        }
    }

    #[test]
    fn preview_metadata_preserves_unicode_offsets_and_long_values() {
        assert_eq!(super::html_title("İ<title>é</title>"), Some("é".into()));
        let title = "é".repeat(300);
        let html = format!("<meta property=\"og:title\" content=\"{title}\">");
        assert_eq!(super::og_meta(&html, "og:title"), Some(title));
        assert_eq!(super::og_meta("<meta data-note=\"İ\" property=\"og:title\" content=\"é\">", "og:title"), Some("é".into()));
    }

    #[test]
    fn preview_metadata_reads_complete_attributes() {
        let html = "<metadata property=\"og:title\" content=\"wrong tag\"><meta data-property=\"og:title\" content=\"wrong attribute\"><meta PROPERTY = 'og:title' data-content='wrong content' CONTENT = 'right > title'>";
        assert_eq!(super::og_meta(html, "og:title"), Some("right > title".into()));
        assert_eq!(super::og_meta("<meta name=twitter:title content=unquoted>", "twitter:title"), Some("unquoted".into()));
        assert_eq!(super::og_meta("<meta property='article:tag' name='twitter:title' content='Correct'>", "twitter:title"), Some("Correct".into()));
        assert_eq!(super::html_title("<titlebar>wrong</titlebar><TITLE lang='en'>Correct</TITLE>"), Some("Correct".into()));
    }

    #[test]
    fn preview_images_resolve_against_the_final_page_url() {
        let base = reqwest::Url::parse("https://example.test/articles/page").unwrap();
        assert_eq!(super::resolve_thumbnail_url(&base, "../images/cover.png"),
            Some("https://example.test/images/cover.png".into()));
        assert_eq!(super::resolve_thumbnail_url(&base, "//cdn.example.test/cover.png"),
            Some("https://cdn.example.test/cover.png".into()));
        assert_eq!(super::resolve_thumbnail_url(&base, "data:image/png;base64,AAAA"), None);
        assert_eq!(super::resolve_thumbnail_url(&base, "  "), None);
        assert_eq!(super::html_thumbnail_url(
            "<meta property='og:image' content=''><meta name='twitter:image' content='/cover.png'>", &base),
            Some("https://example.test/cover.png".into()));
    }

    #[tokio::test]
    async fn preview_metadata_finishes_without_fetching_thumbnail() {
        for mode in ["html", "image-type", "direct"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let thumbnail = format!("{base}/image.png");
            let page = if mode == "direct" { thumbnail.clone() } else { format!("{base}/page") };
            let expected_image = if mode == "html" { thumbnail.clone() } else { page.clone() };
            let html = format!("<title>Ready title</title><meta property='og:image' content='{thumbnail}'>");
            let server = tokio::spawn(async move {
                if mode != "direct" {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                        let mut chunk = [0; 1024];
                        let count = stream.read(&mut chunk).await.unwrap();
                        assert!(count > 0);
                        request.extend_from_slice(&chunk[..count]);
                        assert!(request.len() <= 16 * 1024);
                    }
                    let (content_type, body) = if mode == "html" {
                        ("text/html", html.as_str())
                    } else {
                        ("image/png", "")
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len(),
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                }
                listener
            });
            let requests = LinkPreviewRequests::default();
            let request = requests.begin(&page).unwrap();
            let request_url = page.clone();
            let (events, mut received) = tokio::sync::mpsc::channel(2);
            let fetch = tokio::spawn(async move {
                let _request = request;
                fetch_link_preview(&request_url, &events).await;
            });
            let preview = tokio::time::timeout(Duration::from_secs(2), received.recv()).await
                .expect("metadata must not wait for the thumbnail").unwrap();
            let crust_core::AppEvent::LinkPreviewReady { title, thumbnail_url, .. } = preview else {
                panic!("expected link metadata");
            };
            assert_eq!(thumbnail_url.as_deref(), Some(expected_image.as_str()), "{mode}");
            assert_eq!(title.as_deref(), (mode == "html").then_some("Ready title"), "{mode}");
            tokio::time::timeout(Duration::from_secs(2), fetch).await
                .expect("metadata fetch must finish without loading image bytes").unwrap();
            assert!(requests.begin(&page).is_some(), "metadata completion releases its request");
            assert!(received.try_recv().is_err(), "metadata must not publish image completions");
            let listener = server.await.unwrap();
            assert!(tokio::time::timeout(Duration::from_millis(25), listener.accept()).await.is_err(),
                "metadata must not start a second thumbnail request: {mode}");
        }
    }
}

/// Fetch YouTube video metadata via the public oEmbed JSON endpoint.
/// Returns `Some(AppEvent)` on success, `None` on failure.
async fn fetch_youtube_oembed(
    original_url: &str,
    client: &reqwest::Client,
) -> Option<AppEvent> {
    let oembed_url = format!(
        "https://www.youtube.com/oembed?url={}&format=json",
        url_percent_encode(original_url)
    );
    let resp = client.get(&oembed_url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value =
        crust_emotes::providers::read_bounded_json(resp, MAX_PREVIEW_HTML_BYTES).await?;
    let title = json["title"].as_str().map(|s| s.to_owned());
    let author = json["author_name"].as_str().map(|s| s.to_owned());
    let thumbnail_url = json["thumbnail_url"].as_str().map(|s| s.to_owned());

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
    let mut parsed = web_url(url)?;
    let host = parsed.host_str()?;
    if (host_matches(host, "twitter.com") || host_matches(host, "x.com"))
        && parsed.path().to_ascii_lowercase().contains("/status/")
    {
        parsed.set_host(Some("fxtwitter.com")).ok()?;
        return Some(parsed.to_string());
    }
    None
}

/// Heuristic site-name detection from the URL hostname when og:site_name
/// is missing from the HTML.
fn detect_site_name(url: &str) -> Option<String> {
    let parsed = web_url(url)?;
    let host = parsed.host_str()?;
    let matches = |domain| host_matches(host, domain);
    let name = if matches("youtube.com") || matches("youtu.be") {
        "YouTube"
    } else if matches("twitter.com") || matches("x.com") || matches("fxtwitter.com") {
        "Twitter"
    } else if matches("clips.twitch.tv") {
        "Twitch Clip"
    } else if matches("twitch.tv") {
        "Twitch"
    } else if matches("reddit.com") {
        "Reddit"
    } else if matches("instagram.com") {
        "Instagram"
    } else if matches("tiktok.com") {
        "TikTok"
    } else if matches("github.com") {
        "GitHub"
    } else if matches("wikipedia.org") {
        "Wikipedia"
    } else if matches("steamcommunity.com") || matches("store.steampowered.com") {
        "Steam"
    } else {
        return None;
    };
    Some(name.to_owned())
}

/// Find a complete tag without treating quoted greater-than signs as its end.
fn find_html_tag(html: &str, lower: &str, name: &str, mut offset: usize) -> Option<(usize, usize)> {
    let needle = format!("<{name}");
    while let Some(relative) = lower[offset..].find(&needle) {
        let start = offset + relative;
        let after_name = start + needle.len();
        offset = after_name;
        if !html.as_bytes().get(after_name).is_some_and(|byte| {
            byte.is_ascii_whitespace() || matches!(byte, b'>' | b'/')
        }) {
            continue;
        }
        let mut quote = None;
        for (index, byte) in html.as_bytes().iter().copied().enumerate().skip(after_name) {
            match quote {
                Some(delimiter) if byte == delimiter => quote = None,
                Some(_) => {}
                None if byte == b'\'' || byte == b'"' => quote = Some(byte),
                None if byte == b'>' => return Some((start, index + 1)),
                None => {}
            }
        }
        return None;
    }
    None
}

/// Extract the content of a `<meta property=\"{prop}\" ...>` or `<meta name=\"{prop}\" ...>` tag.
fn og_meta(html: &str, prop: &str) -> Option<String> {
    // ASCII casing preserves every byte offset in the original UTF-8 text.
    let lower = html.to_ascii_lowercase();
    let mut offset = 0;
    while let Some((start, end)) = find_html_tag(html, &lower, "meta", offset) {
        let tag = &html[start..end];
        let matches = ["property", "name"].into_iter().any(|attribute| {
            html_attr(tag, attribute).is_some_and(|value| value.eq_ignore_ascii_case(prop))
        });
        if matches {
            if let Some(val) = html_attr(tag, "content") {
                return Some(html_entities(val));
            }
        }
        offset = end;
    }
    None
}

/// Extract an attribute value from an HTML tag snippet.
fn html_attr<'a>(tag: &'a str, attr: &str) -> Option<&'a str> {
    let bytes = tag.as_bytes();
    let mut position = 0;
    while position < bytes.len() {
        while position < bytes.len()
            && (bytes[position].is_ascii_whitespace() || matches!(bytes[position], b'<' | b'>' | b'/'))
        {
            position += 1;
        }
        let name_start = position;
        while position < bytes.len()
            && !bytes[position].is_ascii_whitespace()
            && !matches!(bytes[position], b'=' | b'>' | b'/')
        {
            position += 1;
        }
        let name = &tag[name_start..position];
        while position < bytes.len() && bytes[position].is_ascii_whitespace() {
            position += 1;
        }
        if bytes.get(position) != Some(&b'=') {
            continue;
        }
        position += 1;
        while position < bytes.len() && bytes[position].is_ascii_whitespace() {
            position += 1;
        }
        let delimiter = bytes.get(position).copied()?;
        let value_start;
        let value_end;
        if delimiter == b'\'' || delimiter == b'"' {
            position += 1;
            value_start = position;
            let length = bytes[position..].iter().position(|byte| *byte == delimiter)?;
            position += length;
            value_end = position;
            position += 1;
        } else {
            value_start = position;
            while position < bytes.len() && !bytes[position].is_ascii_whitespace() && bytes[position] != b'>' {
                position += 1;
            }
            value_end = position;
        }
        if name.eq_ignore_ascii_case(attr) {
            return Some(&tag[value_start..value_end]);
        }
    }
    None
}

/// Extract `<title>` text as a fallback.
fn html_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let (_, body_start) = find_html_tag(html, &lower, "title", 0)?;
    let (body_end, _) = find_html_tag(html, &lower, "/title", body_start)?;
    let text = html[body_start..body_end].trim();
    if text.is_empty() {
        None
    } else {
        Some(html_entities(text))
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

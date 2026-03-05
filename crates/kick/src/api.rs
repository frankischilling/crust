use reqwest::header::{ACCEPT, USER_AGENT};
use serde::Deserialize;
use std::collections::HashMap;
use tracing::{debug, warn};

use crate::KickError;

const KICK_API_V2: &str = "https://kick.com/api/v2/channels";
const KICK_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

#[derive(Debug, Clone)]
pub struct KickChannelInfo {
    pub chatroom_id: u64,
    pub slug: String,
    pub user_id: u64,
    pub username: String,
    pub is_live: bool,
    pub stream_title: Option<String>,
    /// Best-effort badge image lookup map discovered from Kick API payloads.
    /// Keys are either `"<badge_type>"` or `"<badge_type>:<version>"`.
    pub badge_urls: HashMap<String, String>,
}

#[derive(Deserialize)]
struct V2ChannelResponse {
    #[allow(dead_code)]
    id: Option<u64>,
    slug: Option<String>,
    user: Option<V2User>,
    chatroom: Option<V2Chatroom>,
    livestream: Option<V2Livestream>,
}

#[derive(Deserialize)]
struct V2User {
    id: Option<u64>,
    username: Option<String>,
}

#[derive(Deserialize)]
struct V2Chatroom {
    id: Option<u64>,
}

#[derive(Deserialize)]
struct V2Livestream {
    session_title: Option<String>,
    is_live: Option<bool>,
}

/// Fetch channel information from Kick's API to resolve the chatroom_id
/// needed for Pusher WebSocket subscriptions.
pub async fn fetch_channel_info(slug: &str) -> Result<KickChannelInfo, KickError> {
    let url = format!("{KICK_API_V2}/{slug}");
    debug!("Fetching Kick channel info: {url}");

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header(USER_AGENT, KICK_UA)
        .header(ACCEPT, "application/json")
        .send()
        .await?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(KickError::ChannelNotFound(slug.to_owned()));
    }

    let body = resp.error_for_status()?.text().await?;
    let data: V2ChannelResponse = serde_json::from_str(&body)?;
    let raw: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let badge_urls = extract_badge_urls(&raw);

    let chatroom_id = data
        .chatroom
        .and_then(|c| c.id)
        .ok_or_else(|| KickError::ChannelNotFound(slug.to_owned()))?;

    let user = data.user.unwrap_or(V2User {
        id: None,
        username: None,
    });
    let livestream = data.livestream;

    Ok(KickChannelInfo {
        chatroom_id,
        slug: data.slug.unwrap_or_else(|| slug.to_owned()),
        user_id: user.id.unwrap_or(0),
        username: user.username.unwrap_or_else(|| slug.to_owned()),
        is_live: livestream.as_ref().and_then(|l| l.is_live).unwrap_or(false),
        stream_title: livestream.and_then(|l| l.session_title),
        badge_urls,
    })
}

/// Send a chat message to a Kick channel using the official public API.
/// Requires a valid OAuth access token with `chat:write` scope.
pub async fn send_chat_message(
    access_token: &str,
    broadcaster_user_id: u64,
    content: &str,
) -> Result<(), KickError> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "broadcaster_user_id": broadcaster_user_id,
        "content": content,
        "type": "user",
    });

    let resp = client
        .post("https://api.kick.com/public/v1/chat")
        .header(USER_AGENT, KICK_UA)
        .header(ACCEPT, "application/json")
        .header("Authorization", format!("Bearer {access_token}"))
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        warn!("Kick send_chat_message failed ({status}): {text}");
        return Err(KickError::ChannelNotFound(format!("send failed: {status}")));
    }

    Ok(())
}

fn extract_badge_urls(root: &serde_json::Value) -> HashMap<String, String> {
    let mut out = HashMap::new();
    walk_badge_nodes(root, &mut out);
    out
}

fn walk_badge_nodes(v: &serde_json::Value, out: &mut HashMap<String, String>) {
    match v {
        serde_json::Value::Object(map) => {
            let name = find_ci_string(
                map,
                &["type", "badge_type", "badgeType", "name", "slug", "id"],
            )
            .map(|s| s.trim().to_lowercase());
            let version = find_ci_string(map, &["text", "version", "months", "count", "tier"])
                .map(|s| s.trim().to_owned());
            let url = extract_url_from_obj(map);

            if let (Some(name), Some(url)) = (name, url) {
                out.entry(name.clone()).or_insert_with(|| url.clone());
                if let Some(version) = version {
                    if !version.is_empty() {
                        out.entry(format!("{name}:{version}")).or_insert(url);
                    }
                }
            } else if let (Some(months), Some(url)) =
                (find_ci_u64(map, &["months"]), extract_url_from_obj(map))
            {
                // Channel subscriber badge collections usually expose month tiers.
                out.entry(format!("subscriber:{months}")).or_insert(url);
            }

            for child in map.values() {
                walk_badge_nodes(child, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                walk_badge_nodes(item, out);
            }
        }
        _ => {}
    }
}

fn extract_url_from_obj(map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    for key in ["badge_url", "image_url", "icon_url", "url", "src", "srcset"] {
        if let Some(s) = find_ci_string(map, &[key]) {
            let raw = parse_srcset_first_url(s.trim());
            if looks_like_url(raw) {
                return Some(normalize_kick_asset_url(raw));
            }
        }
    }
    None
}

fn parse_srcset_first_url(s: &str) -> &str {
    let first = s.split(',').next().unwrap_or(s).trim();
    first.split_whitespace().next().unwrap_or(first)
}

fn find_ci_string<'a>(
    map: &'a serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<&'a str> {
    for (k, v) in map {
        if keys.iter().any(|want| k.eq_ignore_ascii_case(want)) {
            if let Some(s) = v.as_str() {
                if !s.trim().is_empty() {
                    return Some(s);
                }
            }
        }
    }
    None
}

fn find_ci_u64(map: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<u64> {
    for (k, v) in map {
        if keys.iter().any(|want| k.eq_ignore_ascii_case(want)) {
            if let Some(n) = v.as_u64() {
                return Some(n);
            }
            if let Some(s) = v.as_str() {
                if let Ok(n) = s.trim().parse::<u64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with("//")
        || s.starts_with('/')
}

fn normalize_kick_asset_url(url: &str) -> String {
    if url.starts_with("//") {
        format!("https:{url}")
    } else if url.starts_with('/') {
        format!("https://kick.com{url}")
    } else {
        url.to_owned()
    }
}

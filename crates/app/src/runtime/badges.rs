use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crust_core::events::AppEvent;
use crust_emotes::cache::EmoteCache;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::assets::fetch_emote_image;

/// Shared badge map: (scope, set_name, version) -> image URL.
/// `scope` is "" for global badges, or the channel name for channel-specific badges.
pub(crate) type BadgeMap = Arc<RwLock<HashMap<(String, String, String), String>>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BadgeCacheEntry {
    scope: String,
    name: String,
    version: String,
    url: String,
}

fn badge_cache_path() -> Option<PathBuf> {
    let dirs = ProjectDirs::from("dev", "crust", "crust")?;
    Some(dirs.cache_dir().join("badges").join("badge_map.json"))
}

fn parse_badges_on_large_stack<T>(name: &str, f: impl FnOnce() -> T + Send + 'static) -> Option<T>
where
    T: Send + 'static,
{
    let builder = std::thread::Builder::new()
        .name(name.to_owned())
        .stack_size(16 * 1024 * 1024);
    let handle = match builder.spawn(f) {
        Ok(handle) => handle,
        Err(e) => {
            warn!("Failed to spawn badge parsing thread {name}: {e}");
            return None;
        }
    };

    match handle.join() {
        Ok(v) => Some(v),
        Err(_) => {
            warn!("Badge parsing thread {name} panicked");
            None
        }
    }
}

pub(crate) fn load_badge_map_cache_into(map: &BadgeMap) -> usize {
    let Some(path) = badge_cache_path() else {
        return 0;
    };
    let data = match std::fs::read_to_string(&path) {
        Ok(v) => v,
        Err(_) => return 0,
    };

    let parsed = match serde_json::from_str::<Vec<BadgeCacheEntry>>(&data) {
        Ok(v) => v,
        Err(e) => {
            warn!("Failed to parse badge cache {:?}: {e}", path);
            return 0;
        }
    };

    let mut guard = map.write().unwrap();
    for entry in &parsed {
        guard.insert(
            (
                entry.scope.clone(),
                entry.name.clone(),
                entry.version.clone(),
            ),
            entry.url.clone(),
        );
    }
    parsed.len()
}

pub(crate) fn persist_badge_map_cache(map: &BadgeMap) {
    let Some(path) = badge_cache_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };

    let snapshot: Vec<BadgeCacheEntry> = {
        let guard = map.read().unwrap();
        guard
            .iter()
            .map(|((scope, name, version), url)| BadgeCacheEntry {
                scope: scope.clone(),
                name: name.clone(),
                version: version.clone(),
                url: url.clone(),
            })
            .collect()
    };

    let payload = match serde_json::to_vec(&snapshot) {
        Ok(v) => v,
        Err(e) => {
            warn!("Failed to serialize badge cache: {e}");
            return;
        }
    };

    if let Err(e) = std::fs::create_dir_all(parent) {
        warn!("Failed to create badge cache dir {:?}: {e}", parent);
        return;
    }

    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, payload) {
        warn!("Failed writing temporary badge cache {:?}: {e}", tmp);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        warn!("Failed replacing badge cache {:?}: {e}", path);
    }
}

/// Crust-owned bundled Twitch global badge dataset.
const CRUST_TWITCH_BADGES_JSON: &str = include_str!("../../resources/twitch-badges.json");

/// Resolve a badge image URL from the badge map.
///
/// Twitch IRC sends some badge versions as cumulative counts (e.g.
/// `subscriber/28`, `bits/5000`) that don't directly match the fixed tier
/// version keys stored by the badge API (e.g. "0", "3", "6", "24").
/// When an exact match is not found this function falls back to the highest
/// available version that is numerically <= the requested version.
pub(crate) fn resolve_badge_url(
    map: &HashMap<(String, String, String), String>,
    scope: &str,
    name: &str,
    version: &str,
) -> Option<String> {
    // Try channel-specific scope first, then fall back to global.
    let scopes: &[&str] = if scope.is_empty() {
        &[""]
    } else {
        &[scope, ""]
    };
    for s in scopes {
        // Fast path: exact match.
        if let Some(url) = map.get(&(s.to_string(), name.to_owned(), version.to_owned())) {
            return Some(url.clone());
        }
        // Slow path: numeric fallback - find the highest available version <= version.
        if let Ok(target) = version.parse::<u64>() {
            let mut best: Option<(u64, &String)> = None;
            for ((sc, n, v), url) in map {
                if sc.as_str() == *s && n == name {
                    if let Ok(candidate) = v.parse::<u64>() {
                        if candidate <= target && best.map_or(true, |(b, _)| candidate > b) {
                            best = Some((candidate, url));
                        }
                    }
                }
            }
            if let Some((_, url)) = best {
                return Some(url.clone());
            }
        }
    }
    None
}

/// Parse IVR badge response (flat JSON array) and insert into the badge map.
fn parse_ivr_badge_response(
    body: &str,
    scope: &str,
    map: &mut HashMap<(String, String, String), String>,
) {
    #[derive(serde::Deserialize)]
    struct Version {
        id: String,
        image_url_1x: String,
        #[serde(default)]
        image_url_2x: Option<String>,
        #[serde(default)]
        image_url_4x: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct BadgeSet {
        set_id: String,
        versions: Vec<Version>,
    }

    #[derive(serde::Deserialize)]
    struct CrustBadgeVersion {
        id: String,
        image: String,
    }

    fn normalize_badge_url(url: String) -> String {
        let url = if url.starts_with("//") {
            format!("https:{url}")
        } else {
            url
        };

        // Some badge datasets use base URLs that end in `/` and require a
        // scale suffix (`/1`, `/2`, `/3`). Use `/3` for sharper rendering.
        if url.contains("/badges/v1/") {
            // Avoid the more complex trim/search helpers here; this code runs
            // during startup while badge caches are loaded.
            if url.as_bytes().last() == Some(&b'/') {
                let trimmed = &url[..url.len() - 1];
                let tail = trimmed.rsplit('/').next().unwrap_or("");
                let has_explicit_scale = matches!(tail, "1" | "2" | "3" | "4");
                if !has_explicit_scale {
                    return format!("{trimmed}/3");
                }
            }
        }

        url
    }

    fn badge_set_aliases(set_id: &str) -> Vec<String> {
        let mut out = Vec::with_capacity(3);
        out.push(set_id.to_owned());

        if set_id.contains('-') {
            let alt = set_id.replace('-', "_");
            if !out.iter().any(|v| v == &alt) {
                out.push(alt);
            }
        }
        if set_id.contains('_') {
            let alt = set_id.replace('_', "-");
            if !out.iter().any(|v| v == &alt) {
                out.push(alt);
            }
        }

        out
    }

    let scope = scope.to_owned();

    // IVR classic shape: [{ set_id, versions: [...] }]
    if let Ok(sets) = serde_json::from_str::<Vec<BadgeSet>>(body) {
        for set in sets {
            for ver in set.versions {
                let version_id = ver.id;
                let url = normalize_badge_url(
                    ver.image_url_4x
                        .or(ver.image_url_2x)
                        .unwrap_or(ver.image_url_1x),
                );

                for set_id in badge_set_aliases(&set.set_id) {
                    map.insert((scope.clone(), set_id, version_id.clone()), url.clone());
                }
            }
        }
        return;
    }

    // Bundled shape: { "set_id": [{ id, image, ... }, ...], ... }
    if let Ok(sets) = serde_json::from_str::<HashMap<String, Vec<CrustBadgeVersion>>>(body) {
        for (set_id, versions) in sets {
            for ver in versions {
                let url = normalize_badge_url(ver.image);
                for alias in badge_set_aliases(&set_id) {
                    map.insert((scope.clone(), alias, ver.id.clone()), url.clone());
                }
            }
        }
    }
}

/// Parse Twitch legacy badges.v1 response and insert into the badge map.
fn parse_badges_v1_response(
    body: &str,
    scope: &str,
    map: &mut HashMap<(String, String, String), String>,
) {
    #[derive(serde::Deserialize)]
    struct Version {
        #[serde(default)]
        image_url_1x: Option<String>,
        #[serde(default)]
        image_url_2x: Option<String>,
        #[serde(default)]
        image_url_4x: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct BadgeSet {
        versions: HashMap<String, Version>,
    }

    #[derive(serde::Deserialize)]
    struct Payload {
        badge_sets: HashMap<String, BadgeSet>,
    }

    fn normalize_badge_url(url: String) -> String {
        if url.starts_with("//") {
            format!("https:{url}")
        } else {
            url
        }
    }

    fn badge_set_aliases(set_id: &str) -> Vec<String> {
        let mut out = Vec::with_capacity(3);
        out.push(set_id.to_owned());
        if set_id.contains('-') {
            let alt = set_id.replace('-', "_");
            if !out.iter().any(|v| v == &alt) {
                out.push(alt);
            }
        }
        if set_id.contains('_') {
            let alt = set_id.replace('_', "-");
            if !out.iter().any(|v| v == &alt) {
                out.push(alt);
            }
        }
        out
    }

    let payload = match serde_json::from_str::<Payload>(body) {
        Ok(v) => v,
        Err(_) => return,
    };

    let scope = scope.to_owned();
    for (set_id, set) in payload.badge_sets {
        for (version_id, ver) in set.versions {
            let url = ver
                .image_url_4x
                .or(ver.image_url_2x)
                .or(ver.image_url_1x)
                .map(normalize_badge_url);
            let Some(url) = url else {
                continue;
            };
            for alias in badge_set_aliases(&set_id) {
                map.insert((scope.clone(), alias, version_id.clone()), url.clone());
            }
        }
    }
}

fn parse_helix_badge_response(
    body: &str,
    scope: &str,
    map: &mut HashMap<(String, String, String), String>,
) {
    #[derive(serde::Deserialize)]
    struct Version {
        id: String,
        image_url_1x: String,
        #[serde(default)]
        image_url_2x: Option<String>,
        #[serde(default)]
        image_url_4x: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct BadgeSet {
        set_id: String,
        versions: Vec<Version>,
    }
    #[derive(serde::Deserialize)]
    struct Payload {
        data: Vec<BadgeSet>,
    }

    fn normalize_badge_url(url: String) -> String {
        if url.starts_with("//") {
            format!("https:{url}")
        } else {
            url
        }
    }

    fn badge_set_aliases(set_id: &str) -> Vec<String> {
        let mut out = Vec::with_capacity(3);
        out.push(set_id.to_owned());
        if set_id.contains('-') {
            let alt = set_id.replace('-', "_");
            if !out.iter().any(|v| v == &alt) {
                out.push(alt);
            }
        }
        if set_id.contains('_') {
            let alt = set_id.replace('_', "-");
            if !out.iter().any(|v| v == &alt) {
                out.push(alt);
            }
        }
        out
    }

    let payload = match serde_json::from_str::<Payload>(body) {
        Ok(v) => v,
        Err(_) => return,
    };

    let scope = scope.to_owned();
    for set in payload.data {
        for ver in set.versions {
            let url = normalize_badge_url(
                ver.image_url_4x
                    .or(ver.image_url_2x)
                    .unwrap_or(ver.image_url_1x),
            );
            for alias in badge_set_aliases(&set.set_id) {
                map.insert((scope.clone(), alias, ver.id.clone()), url.clone());
            }
        }
    }
}

fn normalize_oauth_token(token: Option<&str>) -> Option<String> {
    let raw = token?.trim();
    if raw.is_empty() {
        return None;
    }
    Some(raw.strip_prefix("oauth:").unwrap_or(raw).to_owned())
}

async fn helix_auth_from_token(oauth_token: Option<&str>) -> Option<(String, String)> {
    let bearer = normalize_oauth_token(oauth_token)?;
    match crate::validate_token(&bearer).await {
        Ok(info) => Some((info.client_id, bearer)),
        Err(e) => {
            warn!("Helix auth setup failed while validating token: {e}");
            None
        }
    }
}

async fn load_global_badges_v1_fallback(
    badge_map: &BadgeMap,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    if !host_resolves("badges.twitch.tv").await {
        warn_badges_twitch_unresolved_once();
        return;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("crust-badges/1.0")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    if let Some(text) = fetch_badge_payload_with_retries(
        &client,
        &["https://badges.twitch.tv/v1/badges/global/display"],
        "badges.twitch.tv global fallback",
        None,
    )
    .await
    {
        let badge_map_for_parse = badge_map.clone();
        let new_urls = parse_badges_on_large_stack("badge-global-v1-parse", move || {
            let mut map = badge_map_for_parse.write().unwrap();
            let before: HashSet<String> = map.values().cloned().collect();
            parse_badges_v1_response(&text, "", &mut map);
            map.values()
                .filter(|u| !before.contains(*u))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
        if !new_urls.is_empty() {
            info!(
                "Loaded {} global badges via badges.twitch.tv fallback",
                new_urls.len()
            );
            prefetch_badge_images(new_urls, cache, evt_tx);
            persist_badge_map_cache(&badge_map);
        }
    }
}

async fn load_global_badges_helix(
    badge_map: &BadgeMap,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
    oauth_token: Option<&str>,
) {
    let Some((client_id, token)) = helix_auth_from_token(oauth_token).await else {
        return;
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("crust-badges/1.0")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    if let Some(text) = fetch_badge_payload_with_retries(
        &client,
        &["https://api.twitch.tv/helix/chat/badges/global"],
        "Helix global badges",
        Some((client_id.as_str(), token.as_str())),
    )
    .await
    {
        let badge_map_for_parse = badge_map.clone();
        let new_urls = parse_badges_on_large_stack("badge-global-helix-parse", move || {
            let mut map = badge_map_for_parse.write().unwrap();
            let before: HashSet<String> = map.values().cloned().collect();
            parse_helix_badge_response(&text, "", &mut map);
            map.values()
                .filter(|u| !before.contains(*u))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
        if !new_urls.is_empty() {
            info!("Loaded {} global badges via Helix", new_urls.len());
            prefetch_badge_images(new_urls, cache, evt_tx);
            persist_badge_map_cache(badge_map);
        }
    }
}

async fn load_global_badges_ivr_fallback(
    badge_map: &BadgeMap,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("crust-badges/1.0")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    if let Some(text) = fetch_badge_payload_with_retries(
        &client,
        &["https://api.ivr.fi/v2/twitch/badges/global"],
        "IVR global badges",
        None,
    )
    .await
    {
        let badge_map_for_parse = badge_map.clone();
        let new_urls = parse_badges_on_large_stack("badge-global-ivr-parse", move || {
            let mut map = badge_map_for_parse.write().unwrap();
            let before: HashSet<String> = map.values().cloned().collect();
            parse_ivr_badge_response(&text, "", &mut map);
            map.values()
                .filter(|u| !before.contains(*u))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
        if !new_urls.is_empty() {
            info!("Loaded {} global badges via IVR", new_urls.len());
            prefetch_badge_images(new_urls, cache, evt_tx);
            persist_badge_map_cache(badge_map);
        }
    }
}

async fn load_channel_badges_v1_fallback(
    room_id: &str,
    channel: &str,
    badge_map: &BadgeMap,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    if !host_resolves("badges.twitch.tv").await {
        warn_badges_twitch_unresolved_once();
        return;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("crust-badges/1.0")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let url = format!("https://badges.twitch.tv/v1/badges/channels/{room_id}/display");
    if let Some(text) = fetch_badge_payload_with_retries(
        &client,
        &[url.as_str()],
        &format!("badges.twitch.tv channel fallback (room={room_id})"),
        None,
    )
    .await
    {
        let channel = channel.to_owned();
        let badge_map_for_parse = badge_map.clone();
        let new_urls = parse_badges_on_large_stack("badge-channel-v1-parse", move || {
            let mut map = badge_map_for_parse.write().unwrap();
            let before: HashSet<String> = map.values().cloned().collect();
            parse_badges_v1_response(&text, &channel, &mut map);
            map.values()
                .filter(|u| !before.contains(*u))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
        if !new_urls.is_empty() {
            info!(
                "Loaded {} channel badges for room {room_id} via badges.twitch.tv fallback",
                new_urls.len()
            );
            prefetch_badge_images(new_urls, cache, evt_tx);
            persist_badge_map_cache(&badge_map);
        }
    }
}

/// Load global Twitch badges via Helix/IVR with bundled fallback.
pub(crate) async fn load_global_badges(
    badge_map: &BadgeMap,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
    oauth_token: Option<String>,
) {
    let badge_map_for_parse = badge_map.clone();
    let new_urls = parse_badges_on_large_stack("badge-global-bundled-parse", move || {
        let mut map = badge_map_for_parse.write().unwrap();
        let before: HashSet<String> = map.values().cloned().collect();
        parse_ivr_badge_response(CRUST_TWITCH_BADGES_JSON, "", &mut map);
        map.values()
            .filter(|u| !before.contains(*u))
            .cloned()
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();

    if !new_urls.is_empty() {
        info!(
            "Loaded {} global badges from bundled Twitch snapshot",
            new_urls.len()
        );
        prefetch_badge_images(new_urls, cache, evt_tx);
        persist_badge_map_cache(badge_map);
    }

    let badge_map_for_refresh = badge_map.clone();
    let cache = cache.clone();
    let evt_tx = evt_tx.clone();
    let oauth_token_for_refresh = oauth_token.clone();
    tokio::spawn(async move {
        load_global_badges_helix(
            &badge_map_for_refresh,
            &cache,
            &evt_tx,
            oauth_token_for_refresh.as_deref(),
        )
        .await;
        load_global_badges_ivr_fallback(&badge_map_for_refresh, &cache, &evt_tx).await;
        load_global_badges_v1_fallback(&badge_map_for_refresh, &cache, &evt_tx).await;
    });
}

/// Load channel-specific Twitch badges.
pub(crate) async fn load_channel_badges(
    room_id: &str,
    channel: &str,
    badge_map: &BadgeMap,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
    oauth_token: Option<String>,
) {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("crust-badges/1.0")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let helix_auth = helix_auth_from_token(oauth_token.as_deref()).await;
    if let Some((ref client_id, ref token)) = helix_auth {
        let helix = format!("https://api.twitch.tv/helix/chat/badges?broadcaster_id={room_id}");
        if let Some(text) = fetch_badge_payload_with_retries(
            &client,
            &[helix.as_str()],
            &format!("Helix channel badges (room={room_id}, login={channel})"),
            Some((client_id.as_str(), token.as_str())),
        )
        .await
        {
            let channel = channel.to_owned();
            let badge_map_for_parse = badge_map.clone();
            let new_urls = parse_badges_on_large_stack("badge-channel-helix-parse", move || {
                let mut map = badge_map_for_parse.write().unwrap();
                let before: HashSet<String> = map.values().cloned().collect();
                parse_helix_badge_response(&text, &channel, &mut map);
                map.values()
                    .filter(|u| !before.contains(*u))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
            info!(
                "Loaded {} channel badges for room {room_id} via Helix",
                new_urls.len()
            );
            prefetch_badge_images(new_urls, cache, evt_tx);
            persist_badge_map_cache(&badge_map);
            return;
        }
    }

    let ivr_by_id = format!("https://api.ivr.fi/v2/twitch/badges/channel?id={room_id}");
    let ivr_by_login = format!("https://api.ivr.fi/v2/twitch/badges/channel?login={channel}");
    if let Some(text) = fetch_badge_payload_with_retries(
        &client,
        &[ivr_by_id.as_str(), ivr_by_login.as_str()],
        &format!("IVR channel badges (room={room_id}, login={channel})"),
        None,
    )
    .await
    {
        let channel = channel.to_owned();
        let badge_map_for_parse = badge_map.clone();
        let new_urls = parse_badges_on_large_stack("badge-channel-ivr-parse", move || {
            let mut map = badge_map_for_parse.write().unwrap();
            let before: HashSet<String> = map.values().cloned().collect();
            parse_ivr_badge_response(&text, &channel, &mut map);
            map.values()
                .filter(|u| !before.contains(*u))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
        info!(
            "Loaded {} channel badges for room {room_id} via IVR",
            new_urls.len()
        );
        prefetch_badge_images(new_urls, cache, evt_tx);
        persist_badge_map_cache(&badge_map);
    } else {
        load_channel_badges_v1_fallback(room_id, channel, badge_map, cache, evt_tx).await;
    }
}

async fn fetch_badge_payload_with_retries(
    client: &reqwest::Client,
    urls: &[&str],
    label: &str,
    auth: Option<(&str, &str)>,
) -> Option<String> {
    const ATTEMPTS: usize = 3;
    for attempt in 0..ATTEMPTS {
        for url in urls {
            let mut req = client.get(*url);
            if let Some((client_id, bearer)) = auth {
                req = req
                    .header("Client-Id", client_id)
                    .header("Authorization", format!("Bearer {bearer}"));
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => match resp.text().await {
                    Ok(text) => return Some(text),
                    Err(e) => warn!("{label} body-read failed for {url}: {e}"),
                },
                Ok(resp) => warn!("{label} returned HTTP {} for {url}", resp.status()),
                Err(e) => warn!("{label} request failed for {url}: {e}"),
            }
        }
        if attempt + 1 < ATTEMPTS {
            let delay_ms = 300 * (attempt as u64 + 1);
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
    }
    None
}

async fn host_resolves(host: &str) -> bool {
    match tokio::net::lookup_host((host, 443)).await {
        Ok(mut addrs) => addrs.next().is_some(),
        Err(_) => false,
    }
}

fn warn_badges_twitch_unresolved_once() {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        info!("Skipping badges.twitch.tv fallback: host is not resolvable in this environment");
    }
}

/// Eagerly prefetch a list of badge image URLs.
fn prefetch_badge_images(
    urls: Vec<String>,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    if urls.is_empty() {
        return;
    }
    info!("Prefetching {} badge images...", urls.len());
    let _ = evt_tx.try_send(AppEvent::ImagePrefetchQueued { count: urls.len() });
    let sem = Arc::new(tokio::sync::Semaphore::new(20));
    for url in urls {
        let sem = sem.clone();
        let cache = cache.clone();
        let evt_tx = evt_tx.clone();
        tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            fetch_emote_image(&url, &cache, &evt_tx).await;
        });
    }
}

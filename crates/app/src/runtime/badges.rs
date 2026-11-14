use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Shared badge map: (scope, set_name, version) -> image URL.
/// `scope` is "" for global badges, or the channel name for channel-specific badges.
pub(crate) type BadgeDefinitions = HashMap<(String, String, String), String>;
pub(crate) type BadgeMap = Arc<RwLock<BadgeDefinitions>>;

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

struct BadgeCacheWrites {
    pending: Option<BadgeMap>,
    running: bool,
}

struct BadgeCacheWriter {
    writes: Mutex<BadgeCacheWrites>,
}

impl BadgeCacheWriter {
    const fn new() -> Self {
        Self {
            writes: Mutex::new(BadgeCacheWrites {
                pending: None,
                running: false,
            }),
        }
    }

    fn queue(&self, map: &BadgeMap) -> bool {
        let mut writes = self.writes.lock().unwrap();
        writes.pending = Some(map.clone());
        if writes.running {
            return false;
        }
        writes.running = true;
        true
    }

    fn drain(&self, mut write: impl FnMut(&BadgeMap)) {
        loop {
            let map = {
                let mut writes = self.writes.lock().unwrap();
                match writes.pending.take() {
                    Some(map) => map,
                    None => {
                        writes.running = false;
                        return;
                    }
                }
            };
            write(&map);
        }
    }
}

/// Coalesce refreshes without blocking the runtime on serialization or disk I/O.
fn persist_badge_map_cache(map: &BadgeMap) {
    static WRITER: BadgeCacheWriter = BadgeCacheWriter::new();
    if WRITER.queue(map) {
        tokio::task::spawn_blocking(|| WRITER.drain(write_badge_map_cache));
    }
}

fn write_badge_map_cache(map: &BadgeMap) {
    // Capture the current map only after obtaining the cache writer lock. A
    // delayed save must not replace a newer file with a stale queued snapshot.
    static WRITE: Mutex<()> = Mutex::new(());
    let _write = WRITE.lock().unwrap();
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

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), seq));
    if let Err(e) = std::fs::write(&tmp, payload) {
        warn!("Failed writing temporary badge cache {:?}: {e}", tmp);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        warn!("Failed replacing badge cache {:?}: {e}", path);
        let _ = std::fs::remove_file(&tmp);
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
) -> bool {
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
        return true;
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
        return true;
    }
    false
}

/// Parse Twitch legacy badges.v1 response and insert into the badge map.
fn parse_badges_v1_response(
    body: &str,
    scope: &str,
    map: &mut HashMap<(String, String, String), String>,
) -> bool {
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
        Err(_) => return false,
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
    true
}

fn parse_helix_badge_response(
    body: &str,
    scope: &str,
    map: &mut HashMap<(String, String, String), String>,
) -> bool {
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
        Err(_) => return false,
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
    true
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
    match super::auth::validate_token(&bearer).await {
        Ok(info) => Some((info.client_id, bearer)),
        Err(e) => {
            warn!("Helix auth setup failed while validating token: {e}");
            None
        }
    }
}

fn parse_badge_snapshot(
    body: &str,
    scope: &str,
    parse: fn(&str, &str, &mut BadgeDefinitions) -> bool,
) -> Option<BadgeDefinitions> {
    let mut incoming = BadgeDefinitions::new();
    parse(body, scope, &mut incoming).then_some(incoming)
}

// Global fallback sources supplement one another. Parse outside the shared
// lock, then merge only returned keys without cloning every loaded badge URL.
fn merge_badge_response(
    body: &str,
    scope: &str,
    badge_map: &BadgeMap,
    parse: fn(&str, &str, &mut BadgeDefinitions) -> bool,
) -> Option<usize> {
    let incoming = parse_badge_snapshot(body, scope, parse)?;
    let mut map = badge_map.write().unwrap();
    let mut changed = 0;
    for (key, url) in incoming {
        if map.get(&key) != Some(&url) {
            map.insert(key, url);
            changed += 1;
        }
    }
    Some(changed)
}

pub(crate) fn replace_channel_badges(
    channel: &str,
    badge_map: &BadgeMap,
    mut definitions: BadgeDefinitions,
) -> bool {
    definitions.retain(|(scope, _, _), _| scope == channel);
    let mut map = badge_map.write().unwrap();
    let previous_count = map.keys().filter(|(scope, _, _)| scope == channel).count();
    if previous_count == definitions.len()
        && definitions
            .iter()
            .all(|(key, value)| map.get(key) == Some(value))
    {
        return false;
    }
    map.retain(|(scope, _, _), _| scope != channel);
    map.extend(definitions);
    true
}

/// Apply a completed snapshot only after the caller verifies its load generation.
/// Successful empty snapshots clear old channel badges; other scopes are retained.
pub(crate) fn publish_channel_badges(
    channel: &str,
    badge_map: &BadgeMap,
    definitions: BadgeDefinitions,
) {
    if replace_channel_badges(channel, badge_map, definitions) {
        persist_badge_map_cache(badge_map);
    }
}

async fn load_global_badges_v1_fallback(badge_map: &BadgeMap) {
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
        let changed = parse_badges_on_large_stack("badge-global-v1-parse", move || {
            merge_badge_response(&text, "", &badge_map_for_parse, parse_badges_v1_response)
        })
        .flatten()
        .unwrap_or_default();
        if changed != 0 {
            info!("Updated {changed} global badge definitions via badges.twitch.tv fallback");
            persist_badge_map_cache(&badge_map);
        }
    }
}

async fn load_global_badges_helix(badge_map: &BadgeMap, oauth_token: Option<&str>) {
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
        let changed = parse_badges_on_large_stack("badge-global-helix-parse", move || {
            merge_badge_response(&text, "", &badge_map_for_parse, parse_helix_badge_response)
        })
        .flatten()
        .unwrap_or_default();
        if changed != 0 {
            info!("Updated {changed} global badge definitions via Helix");
            persist_badge_map_cache(badge_map);
        }
    }
}

async fn load_global_badges_ivr_fallback(badge_map: &BadgeMap) {
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
        let changed = parse_badges_on_large_stack("badge-global-ivr-parse", move || {
            merge_badge_response(&text, "", &badge_map_for_parse, parse_ivr_badge_response)
        })
        .flatten()
        .unwrap_or_default();
        if changed != 0 {
            info!("Updated {changed} global badge definitions via IVR");
            persist_badge_map_cache(badge_map);
        }
    }
}

async fn load_channel_badges_v1_fallback(room_id: &str, channel: &str) -> Option<BadgeDefinitions> {
    if !host_resolves("badges.twitch.tv").await {
        warn_badges_twitch_unresolved_once();
        return None;
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
        let definitions = parse_badges_on_large_stack("badge-channel-v1-parse", move || {
            parse_badge_snapshot(&text, &channel, parse_badges_v1_response)
        })
        .flatten();
        if let Some(ref definitions) = definitions {
            info!(
                "Loaded {} channel badge definitions for room {room_id} via badges.twitch.tv fallback",
                definitions.len()
            );
        }
        return definitions;
    }
    None
}

/// Load global Twitch badges via Helix/IVR with bundled fallback.
pub(crate) async fn load_global_badges(badge_map: &BadgeMap, oauth_token: Option<String>) {
    let badge_map_for_parse = badge_map.clone();
    let changed = parse_badges_on_large_stack("badge-global-bundled-parse", move || {
        merge_badge_response(
            CRUST_TWITCH_BADGES_JSON,
            "",
            &badge_map_for_parse,
            parse_ivr_badge_response,
        )
    })
    .flatten()
    .unwrap_or_default();

    if changed != 0 {
        info!("Updated {changed} global badge definitions from bundled Twitch snapshot");
        persist_badge_map_cache(badge_map);
    }

    let badge_map_for_refresh = badge_map.clone();
    tokio::spawn(async move {
        load_global_badges_helix(&badge_map_for_refresh, oauth_token.as_deref()).await;
        load_global_badges_ivr_fallback(&badge_map_for_refresh).await;
        load_global_badges_v1_fallback(&badge_map_for_refresh).await;
    });
}

/// Fetch an owned channel snapshot without changing shared state. A valid empty
/// response returns `Some(empty)`; network errors or malformed data return `None`.
pub(crate) async fn load_channel_badges(
    room_id: &str,
    channel: &str,
    oauth_token: Option<String>,
) -> Option<BadgeDefinitions> {
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
            let definitions = parse_badges_on_large_stack("badge-channel-helix-parse", move || {
                parse_badge_snapshot(&text, &channel, parse_helix_badge_response)
            })
            .flatten();
            if let Some(definitions) = definitions {
                info!(
                    "Loaded {} channel badge definitions for room {room_id} via Helix",
                    definitions.len()
                );
                return Some(definitions);
            }
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
        let definitions = parse_badges_on_large_stack("badge-channel-ivr-parse", move || {
            parse_badge_snapshot(&text, &channel, parse_ivr_badge_response)
        })
        .flatten();
        if let Some(definitions) = definitions {
            info!(
                "Loaded {} channel badge definitions for room {room_id} via IVR",
                definitions.len()
            );
            return Some(definitions);
        }
    }
    load_channel_badges_v1_fallback(room_id, channel).await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_cache_writes_coalesce_updates_while_a_save_is_running() {
        let writer = Arc::new(BadgeCacheWriter::new());
        let map = BadgeMap::default();
        let key = ("room".to_owned(), "subscriber".to_owned(), "1".to_owned());
        map.write()
            .unwrap()
            .insert(key.clone(), "initial".to_owned());
        assert!(writer.queue(&map));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = writer.clone();
        let worker_key = key.clone();
        let saved = std::thread::spawn(move || {
            let mut snapshots = Vec::new();
            worker.drain(|map| {
                if snapshots.is_empty() {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                }
                snapshots.push(map.read().unwrap()[&worker_key].clone());
            });
            snapshots
        });
        started_rx.recv().unwrap();
        for update in 0..32 {
            map.write()
                .unwrap()
                .insert(key.clone(), format!("update-{update}"));
            assert!(!writer.queue(&map), "one writer must own all queued saves");
        }
        release_tx.send(()).unwrap();
        assert_eq!(saved.join().unwrap(), ["update-31", "update-31"]);
        assert!(
            writer.queue(&map),
            "later refreshes can start another writer"
        );
        let mut writes = 0;
        writer.drain(|_| writes += 1);
        assert_eq!(writes, 1);
    }

    #[test]
    fn badge_refresh_merges_changed_keys_and_preserves_other_channels() {
        let map = BadgeMap::default();
        let first = r#"[{"set_id":"subscriber","versions":[{"id":"1","image_url_1x":"https://example.test/first.png"}]}]"#;
        let second = r#"[{"set_id":"subscriber","versions":[{"id":"1","image_url_1x":"https://example.test/second.png"}]}]"#;
        assert_eq!(
            merge_badge_response(first, "a", &map, parse_ivr_badge_response),
            Some(1)
        );
        assert_eq!(
            merge_badge_response(first, "b", &map, parse_ivr_badge_response),
            Some(1)
        );
        assert_eq!(
            merge_badge_response(first, "a", &map, parse_ivr_badge_response),
            Some(0)
        );
        assert_eq!(
            merge_badge_response(second, "a", &map, parse_ivr_badge_response),
            Some(1)
        );
        let map = map.read().unwrap();
        assert_eq!(
            resolve_badge_url(&map, "a", "subscriber", "1").as_deref(),
            Some("https://example.test/second.png")
        );
        assert_eq!(
            resolve_badge_url(&map, "b", "subscriber", "1").as_deref(),
            Some("https://example.test/first.png")
        );
    }

    #[test]
    fn badge_refresh_persists_new_keys_even_when_the_image_url_is_already_known() {
        let map = BadgeMap::default();
        let body = r#"[{"set_id":"subscriber","versions":[{"id":"1","image_url_1x":"https://example.test/shared.png"}]}]"#;
        assert_eq!(
            merge_badge_response(body, "a", &map, parse_ivr_badge_response),
            Some(1)
        );
        assert_eq!(
            merge_badge_response(body, "b", &map, parse_ivr_badge_response),
            Some(1)
        );
        assert_eq!(
            merge_badge_response("invalid", "a", &map, parse_ivr_badge_response),
            None
        );
        assert_eq!(map.read().unwrap().len(), 2);
    }

    #[test]
    fn badge_parsers_distinguish_valid_empty_responses_from_invalid_data() {
        let map = BadgeMap::default();
        let responses: [(fn(&str, &str, &mut BadgeDefinitions) -> bool, &str); 4] = [
            (parse_ivr_badge_response, "[]"),
            (parse_ivr_badge_response, "{}"),
            (parse_helix_badge_response, r#"{"data":[]}"#),
            (parse_badges_v1_response, r#"{"badge_sets":{}}"#),
        ];
        for (parse, empty) in responses {
            assert_eq!(merge_badge_response(empty, "room", &map, parse), Some(0));
            assert_eq!(merge_badge_response("not json", "room", &map, parse), None);
            assert_eq!(merge_badge_response("null", "room", &map, parse), None);
            assert_eq!(
                merge_badge_response(r#"{"error":"unavailable"}"#, "room", &map, parse),
                None
            );
        }
        assert!(map.read().unwrap().is_empty());
    }

    #[test]
    fn channel_snapshot_replaces_its_scope_and_clears_removed_badges() {
        let map = BadgeMap::default();
        let key = |scope: &str, version: &str| {
            (
                scope.to_owned(),
                "subscriber".to_owned(),
                version.to_owned(),
            )
        };
        map.write().unwrap().extend([
            (key("", "1"), "global".to_owned()),
            (key("a", "1"), "old".to_owned()),
            (key("a", "2"), "removed".to_owned()),
            (key("b", "1"), "other-channel".to_owned()),
        ]);
        let definitions = BadgeDefinitions::from([(key("a", "1"), "current".to_owned())]);
        assert!(replace_channel_badges("a", &map, definitions.clone()));
        assert!(!replace_channel_badges("a", &map, definitions));
        {
            let snapshot = map.read().unwrap();
            assert_eq!(
                snapshot.get(&key("a", "1")).map(String::as_str),
                Some("current")
            );
            assert!(!snapshot.contains_key(&key("a", "2")));
            assert_eq!(
                snapshot.get(&key("", "1")).map(String::as_str),
                Some("global")
            );
            assert_eq!(
                snapshot.get(&key("b", "1")).map(String::as_str),
                Some("other-channel")
            );
        }
        let empty =
            parse_badge_snapshot(r#"{"data":[]}"#, "a", parse_helix_badge_response).unwrap();
        assert!(replace_channel_badges("a", &map, empty));
        let snapshot = map.read().unwrap();
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.keys().all(|(scope, _, _)| scope != "a"));
    }
}

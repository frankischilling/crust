use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Shared badge map: (scope, set_name, version) -> image URL.
/// `scope` is "" for global badges, or the channel name for channel-specific badges.
pub(crate) type BadgeDefinitions = HashMap<(String, String, String), String>;
pub(crate) type BadgeMap = Arc<RwLock<BadgeDefinitions>>;

fn badge_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(10))
            .user_agent("crust-badges/1.0")
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// Credentials from the reducer's current validated account. Each request owns
/// its snapshot; the HTTP client stores no account or authorization state.
#[derive(Clone)]
pub(crate) struct BadgeAuth {
    client_id: String,
    bearer: String,
}

impl BadgeAuth {
    pub(crate) fn from_validated(client_id: Option<&str>, token: &str) -> Option<Self> {
        let client_id = client_id?.trim();
        let token = token.trim();
        let bearer = token.strip_prefix("oauth:").unwrap_or(token).trim();
        if client_id.is_empty() || bearer.is_empty() {
            return None;
        }
        Some(Self {
            client_id: client_id.to_owned(),
            bearer: bearer.to_owned(),
        })
    }

    fn headers(&self) -> (&str, &str) {
        (&self.client_id, &self.bearer)
    }
}

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

fn parse_badge_snapshot(
    body: &str,
    scope: &str,
    parse: fn(&str, &str, &mut BadgeDefinitions) -> bool,
) -> Option<BadgeDefinitions> {
    let mut incoming = BadgeDefinitions::new();
    parse(body, scope, &mut incoming).then_some(incoming)
}

// The bundled snapshot fills gaps without replacing newer cached definitions.
fn seed_badge_response(
    body: &str,
    scope: &str,
    badge_map: &BadgeMap,
    parse: fn(&str, &str, &mut BadgeDefinitions) -> bool,
) -> Option<usize> {
    let incoming = parse_badge_snapshot(body, scope, parse)?;
    let mut map = badge_map.write().unwrap();
    let mut changed = 0;
    for (key, url) in incoming {
        if let std::collections::hash_map::Entry::Vacant(entry) = map.entry(key) {
            entry.insert(url);
            changed += 1;
        }
    }
    Some(changed)
}

pub(crate) fn replace_badge_scope(
    scope: &str,
    badge_map: &BadgeMap,
    definitions: BadgeDefinitions,
) -> bool {
    replace_badge_definitions(scope, &mut badge_map.write().unwrap(), definitions)
}

fn replace_badge_definitions(
    scope: &str,
    map: &mut BadgeDefinitions,
    mut definitions: BadgeDefinitions,
) -> bool {
    definitions.retain(|(candidate, _, _), _| candidate == scope);
    let previous_count = map.keys().filter(|(candidate, _, _)| candidate == scope).count();
    if previous_count == definitions.len()
        && definitions
            .iter()
            .all(|(key, value)| map.get(key) == Some(value))
    {
        return false;
    }
    map.retain(|(candidate, _, _), _| candidate != scope);
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
    if replace_badge_scope(channel, badge_map, definitions) {
        persist_badge_map_cache(badge_map);
    }
}

async fn load_global_badges_v1_fallback() -> Option<BadgeDefinitions> {
    if !host_resolves("badges.twitch.tv").await {
        warn_badges_twitch_unresolved_once();
        return None;
    }

    if let Some(text) = fetch_badge_payload_with_retries(
        badge_client(),
        &["https://badges.twitch.tv/v1/badges/global/display"],
        "badges.twitch.tv global fallback",
        None,
    )
    .await
    {
        return parse_badges_on_large_stack("badge-global-v1-parse", move || {
            parse_badge_snapshot(&text, "", parse_badges_v1_response)
        })
        .flatten();
    }
    None
}

async fn load_global_badges_from(
    client: &reqwest::Client,
    auth: Option<&BadgeAuth>,
    helix_url: &str,
    ivr_url: &str,
) -> Option<BadgeDefinitions> {
    if let Some(auth) = auth {
        if let Some(text) = fetch_badge_payload_with_retries(
            client,
            &[helix_url],
            "Helix global badges",
            Some(auth.headers()),
        )
        .await
        {
            let definitions = parse_badges_on_large_stack("badge-global-helix-parse", move || {
                parse_badge_snapshot(&text, "", parse_helix_badge_response)
            })
            .flatten();
            if let Some(definitions) = definitions {
                return Some(definitions);
            }
        }
    }

    if let Some(text) = fetch_badge_payload_with_retries(
        client,
        &[ivr_url],
        "IVR global badges",
        None,
    )
    .await
    {
        return parse_badges_on_large_stack("badge-global-ivr-parse", move || {
            parse_badge_snapshot(&text, "", parse_ivr_badge_response)
        })
        .flatten();
    }
    None
}

async fn load_channel_badges_v1_fallback(room_id: &str, channel: &str) -> Option<BadgeDefinitions> {
    if !host_resolves("badges.twitch.tv").await {
        warn_badges_twitch_unresolved_once();
        return None;
    }

    let url = format!("https://badges.twitch.tv/v1/badges/channels/{room_id}/display");
    if let Some(text) = fetch_badge_payload_with_retries(
        badge_client(),
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

/// Own the global refresh so account changes and shutdown cancel its requests.
#[derive(Default)]
pub(crate) struct GlobalBadgeRefresh {
    task: Option<tokio::task::JoinHandle<()>>,
    generation: Arc<AtomicU64>,
}

impl GlobalBadgeRefresh {
    pub(crate) fn request(&mut self, badge_map: &BadgeMap, auth: Option<BadgeAuth>) {
        let badge_map = badge_map.clone();
        self.replace_with(move |generation, expected| async move {
            let definitions = load_global_badges_from(
                badge_client(),
                auth.as_ref(),
                "https://api.twitch.tv/helix/chat/badges/global",
                "https://api.ivr.fi/v2/twitch/badges/global",
            )
            .await;
            let definitions = match definitions {
                Some(definitions) => Some(definitions),
                None => load_global_badges_v1_fallback().await,
            };
            if let Some(definitions) = definitions {
                let count = definitions.len();
                if replace_current_global_badges(&badge_map, definitions, &generation, expected) {
                    info!("Refreshed {count} global badge definitions");
                    persist_badge_map_cache(&badge_map);
                }
            }
        });
    }

    pub(crate) fn ensure_started(&mut self, badge_map: &BadgeMap, auth: Option<BadgeAuth>) {
        // The first connection can reuse startup work. After a disconnect the
        // cleared owner starts a new refresh with the current account snapshot.
        if self.task.is_none() {
            self.request(badge_map, auth);
        }
    }

    fn replace_with<F>(&mut self, refresh: impl FnOnce(Arc<AtomicU64>, u64) -> F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.clear();
        let expected = self.generation.load(Ordering::Relaxed);
        self.task = Some(tokio::spawn(refresh(self.generation.clone(), expected)));
    }

    pub(crate) fn clear(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn replace_current_global_badges(
    badge_map: &BadgeMap,
    definitions: BadgeDefinitions,
    generation: &AtomicU64,
    expected: u64,
) -> bool {
    let mut map = badge_map.write().unwrap();
    // Synchronous parsing can outlive an aborted task's current poll.
    // Check while holding the publication lock so a stale parse cannot replace a newer snapshot.
    generation.load(Ordering::Relaxed) == expected
        && replace_badge_definitions("", &mut map, definitions)
}

impl Drop for GlobalBadgeRefresh {
    fn drop(&mut self) {
        self.clear();
    }
}

pub(crate) fn load_bundled_global_badges(badge_map: &BadgeMap) {
    let badge_map_for_parse = badge_map.clone();
    let changed = parse_badges_on_large_stack("badge-global-bundled-parse", move || {
        seed_badge_response(
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
}

/// Fetch an owned channel snapshot without changing shared state. A valid empty
/// response returns `Some(empty)`; network errors or malformed data return `None`.
pub(crate) async fn load_channel_badges(
    room_id: &str,
    channel: &str,
    auth: Option<BadgeAuth>,
) -> Option<BadgeDefinitions> {
    let urls = ChannelBadgeUrls {
        helix: format!("https://api.twitch.tv/helix/chat/badges?broadcaster_id={room_id}"),
        ivr_by_id: format!("https://api.ivr.fi/v2/twitch/badges/channel?id={room_id}"),
        ivr_by_login: format!("https://api.ivr.fi/v2/twitch/badges/channel?login={channel}"),
    };
    if let Some(definitions) =
        load_channel_badges_from(badge_client(), channel, auth.as_ref(), &urls).await
    {
        return Some(definitions);
    }
    load_channel_badges_v1_fallback(room_id, channel).await
}

struct ChannelBadgeUrls {
    helix: String,
    ivr_by_id: String,
    ivr_by_login: String,
}

async fn load_channel_badges_from(
    client: &reqwest::Client,
    channel: &str,
    auth: Option<&BadgeAuth>,
    urls: &ChannelBadgeUrls,
) -> Option<BadgeDefinitions> {
    if let Some(auth) = auth {
        if let Some(text) = fetch_badge_payload_with_retries(
            client,
            &[&urls.helix],
            &format!("Helix channel badges (login={channel})"),
            Some(auth.headers()),
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
                    "Loaded {} channel badge definitions via Helix",
                    definitions.len()
                );
                return Some(definitions);
            }
        }
    }

    if let Some(text) = fetch_badge_payload_with_retries(
        client,
        &[&urls.ivr_by_id, &urls.ivr_by_login],
        &format!("IVR channel badges (login={channel})"),
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
                "Loaded {} channel badge definitions via IVR",
                definitions.len()
            );
            return Some(definitions);
        }
    }
    None
}

async fn fetch_badge_payload_with_retries(
    client: &reqwest::Client,
    urls: &[&str],
    label: &str,
    auth: Option<(&str, &str)>,
) -> Option<String> {
    const ATTEMPTS: usize = 3;
    let mut retryable = vec![true; urls.len()];
    for attempt in 0..ATTEMPTS {
        for (index, url) in urls.iter().enumerate() {
            if !retryable[index] {
                continue;
            }
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
                Ok(resp) => {
                    let status = resp.status();
                    warn!("{label} returned HTTP {status} for {url}");
                    if status.is_client_error()
                        && status != reqwest::StatusCode::REQUEST_TIMEOUT
                        && status != reqwest::StatusCode::TOO_MANY_REQUESTS
                    {
                        retryable[index] = false;
                    }
                }
                Err(e) => warn!("{label} request failed for {url}: {e}"),
            }
        }
        if !retryable.iter().any(|retry| *retry) {
            break;
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const HELIX_BADGES: &str = r#"{"data":[{"set_id":"subscriber","versions":[{"id":"1","image_url_1x":"https://example.test/badge.png"}]}]}"#;
    const IVR_BADGES: &str = r#"[{"set_id":"subscriber","versions":[{"id":"1","image_url_1x":"https://example.test/badge.png"}]}]"#;

    async fn serve_badges(
        replies: Vec<(u16, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            // All sequential loads must reuse this connection, including after
            // account changes. Authorization belongs to each request header.
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut requests = Vec::new();
            for (status, body) in replies {
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut bytes = [0; 1024];
                    let received = socket.read(&mut bytes).await.unwrap();
                    assert_ne!(received, 0, "badge HTTP connection closed between requests");
                    request.extend_from_slice(&bytes[..received]);
                    assert!(request.len() <= 16 * 1024);
                }
                requests.push(String::from_utf8(request).unwrap());
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: keep-alive\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (base, task)
    }

    fn badge_urls(base: &str, room: usize) -> ChannelBadgeUrls {
        ChannelBadgeUrls {
            helix: format!("{base}/helix?room={room}"),
            ivr_by_id: format!("{base}/ivr?id={room}"),
            ivr_by_login: format!("{base}/ivr?login=room{room}"),
        }
    }

    fn paused_global_refresh(
        refresh: &mut GlobalBadgeRefresh,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        let (started, observed_start) = tokio::sync::oneshot::channel();
        let (release, pending) = tokio::sync::oneshot::channel();
        let (completed, observed_completion) = tokio::sync::oneshot::channel();
        refresh.replace_with(move |_, _| async move {
            let _ = started.send(());
            let _ = pending.await;
            let _ = completed.send(());
        });
        (observed_start, release, observed_completion)
    }

    #[tokio::test]
    async fn global_badge_refresh_replaces_pending_work() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut refresh = GlobalBadgeRefresh::default();
            let (started, mut old_release, old_completion) = paused_global_refresh(&mut refresh);
            started.await.unwrap();

            let (started, release, completion) = paused_global_refresh(&mut refresh);
            started.await.unwrap();
            old_release.closed().await;
            assert!(old_completion.await.is_err());
            release.send(()).unwrap();
            completion.await.unwrap();
        })
        .await
        .expect("replacing a global refresh must cancel the previous request");
    }

    #[tokio::test]
    async fn global_badge_refresh_cancels_on_clear_and_drop() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut refresh = GlobalBadgeRefresh::default();
            let (started, mut release, completion) = paused_global_refresh(&mut refresh);
            started.await.unwrap();
            refresh.clear();
            refresh.clear();
            release.closed().await;
            assert!(completion.await.is_err());

            let (started, mut release, completion) = paused_global_refresh(&mut refresh);
            started.await.unwrap();
            drop(refresh);
            release.closed().await;
            assert!(completion.await.is_err());
        })
        .await
        .expect("clearing or dropping a global refresh must cancel its request");
    }

    #[test]
    fn a_canceled_global_parse_cannot_replace_the_current_snapshot() {
        let mut refresh = GlobalBadgeRefresh::default();
        let map = BadgeMap::default();
        let key = (String::new(), "subscriber".into(), "1".into());
        let old_generation = refresh.generation.load(Ordering::Relaxed);
        refresh.clear();
        let current_generation = refresh.generation.load(Ordering::Relaxed);
        assert!(replace_current_global_badges(&map,
            BadgeDefinitions::from([(key.clone(), "new".into())]),
            &refresh.generation, current_generation));
        assert!(!replace_current_global_badges(&map,
            BadgeDefinitions::from([(key.clone(), "old".into())]),
            &refresh.generation, old_generation));
        assert_eq!(map.read().unwrap()[&key], "new");
        refresh.clear();
        assert!(!replace_current_global_badges(&map, BadgeDefinitions::new(),
            &refresh.generation, current_generation));
        assert_eq!(map.read().unwrap()[&key], "new");
    }

    #[tokio::test]
    async fn authoritative_global_badges_skip_fallbacks_and_replace_only_the_global_scope() {
        for body in [HELIX_BADGES, r#"{"data":[]}"#] {
            let (base, server) = serve_badges(vec![(200, body)]).await;
            let auth = BadgeAuth::from_validated(Some("client"), "private-token").unwrap();
            let map = BadgeMap::default();
            let old = (String::new(), "old".to_owned(), "1".to_owned());
            let channel = ("room".to_owned(), "subscriber".to_owned(), "1".to_owned());
            map.write().unwrap().extend([
                (old.clone(), "old-global".to_owned()),
                (channel.clone(), "channel-badge".to_owned()),
            ]);
            let (definitions, requests) = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                async {
                    let definitions = load_global_badges_from(
                        badge_client(),
                        Some(&auth),
                        &format!("{base}/helix"),
                        &format!("{base}/fallback"),
                    )
                    .await
                    .unwrap();
                    (definitions, server.await.unwrap())
                },
            )
            .await
            .unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].starts_with("GET /helix "));
            assert!(replace_badge_scope("", &map, definitions.clone()));
            assert!(!replace_badge_scope("", &map, definitions));
            let current = map.read().unwrap();
            assert!(!current.contains_key(&old));
            assert_eq!(current[&channel], "channel-badge");
            assert_eq!(current.len(), if body == HELIX_BADGES { 2 } else { 1 });
        }
    }

    #[tokio::test]
    async fn global_badge_fallback_handles_rejected_or_malformed_primary_without_credentials() {
        for (status, body) in [(401, ""), (200, "invalid"), (200, r#"{"data":[{}]}"#)] {
            let (base, server) = serve_badges(vec![(status, body), (200, IVR_BADGES)]).await;
            let auth = BadgeAuth::from_validated(Some("client"), "private-token").unwrap();
            let (definitions, requests) = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                async {
                    let definitions = load_global_badges_from(
                        badge_client(),
                        Some(&auth),
                        &format!("{base}/helix"),
                        &format!("{base}/fallback"),
                    )
                    .await
                    .unwrap();
                    (definitions, server.await.unwrap())
                },
            )
            .await
            .unwrap();
            assert_eq!(definitions.len(), 1);
            assert_eq!(requests.len(), 2);
            assert!(requests[0].starts_with("GET /helix "));
            let fallback = requests[1].to_ascii_lowercase();
            assert!(fallback.starts_with("get /fallback "));
            assert!(!fallback.contains("authorization:"));
            assert!(!fallback.contains("client-id:"));
            assert!(!fallback.contains("private-token"));
        }
    }

    #[tokio::test]
    async fn anonymous_global_badges_skip_the_authenticated_endpoint() {
        let (base, server) = serve_badges(vec![(200, IVR_BADGES)]).await;
        let (definitions, requests) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            async {
                let definitions = load_global_badges_from(
                    badge_client(),
                    None,
                    &format!("{base}/helix"),
                    &format!("{base}/fallback"),
                )
                .await
                .unwrap();
                (definitions, server.await.unwrap())
            },
        )
        .await
        .unwrap();
        assert_eq!(definitions.len(), 1);
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /fallback "));
        assert!(!requests[0].to_ascii_lowercase().contains("authorization:"));
    }

    #[tokio::test]
    async fn badge_loads_reuse_http_connection_and_current_validated_account() {
        let (base, server) = serve_badges(vec![
            (200, HELIX_BADGES),
            (200, HELIX_BADGES),
            (200, HELIX_BADGES),
            (200, IVR_BADGES),
            (200, IVR_BADGES),
        ])
        .await;
        let first = BadgeAuth::from_validated(Some("first-client"), " oauth:first-token ").unwrap();
        let next = BadgeAuth::from_validated(Some("next-client"), "next-token").unwrap();
        let credentials = [
            Some(first.clone()),
            Some(first),
            Some(next),
            BadgeAuth::from_validated(None, "old-saved-token"),
            BadgeAuth::from_validated(Some("next-client"), "oauth:"),
        ];
        let requests = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for (index, auth) in credentials.iter().enumerate() {
                let channel = format!("room{index}");
                let definitions = load_channel_badges_from(
                    badge_client(),
                    &channel,
                    auth.as_ref(),
                    &badge_urls(&base, index),
                )
                .await
                .unwrap();
                assert_eq!(definitions.len(), 1);
                assert!(definitions.keys().all(|(scope, _, _)| scope == &channel));
            }
            server.await.unwrap()
        })
        .await
        .unwrap();

        assert_eq!(
            requests.len(),
            5,
            "one badge request per channel, no validation requests"
        );
        for request in &requests[..2] {
            let request = request.to_ascii_lowercase();
            assert!(request.starts_with("get /helix?"));
            assert!(request.contains("client-id: first-client\r\n"));
            assert!(request.contains("authorization: bearer first-token\r\n"));
        }
        let switched = requests[2].to_ascii_lowercase();
        assert!(switched.contains("client-id: next-client\r\n"));
        assert!(switched.contains("authorization: bearer next-token\r\n"));
        assert!(!switched.contains("first-token"));
        for request in &requests[3..] {
            let request = request.to_ascii_lowercase();
            assert!(request.starts_with("get /ivr?"));
            assert!(!request.contains("authorization:"));
            assert!(!request.contains("client-id:"));
            assert!(!request.contains("old-saved-token"));
        }
    }

    #[tokio::test]
    async fn rejected_badge_auth_falls_back_without_retries_or_forwarded_credentials() {
        for status in [400, 401, 403, 404, 410, 422] {
            let (base, server) = serve_badges(vec![(status, ""), (200, IVR_BADGES)]).await;
            let auth = BadgeAuth::from_validated(Some("client"), "private-token").unwrap();
            let requests = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let definitions = load_channel_badges_from(
                    badge_client(),
                    "room",
                    Some(&auth),
                    &badge_urls(&base, 1),
                )
                .await
                .unwrap();
                assert_eq!(definitions.len(), 1);
                server.await.unwrap()
            })
            .await
            .unwrap();
            assert_eq!(requests.len(), 2);
            assert!(requests[0].starts_with("GET /helix?"));
            let fallback = requests[1].to_ascii_lowercase();
            assert!(fallback.starts_with("get /ivr?"));
            assert!(!fallback.contains("authorization:"));
            assert!(!fallback.contains("client-id:"));
            assert!(!fallback.contains("private-token"));
        }
    }

    #[tokio::test]
    async fn badge_retries_skip_permanent_urls_and_recover_transient_alternatives() {
        for status in [408, 429, 503] {
            let (base, server) =
                serve_badges(vec![(404, ""), (status, ""), (200, IVR_BADGES)]).await;
            let missing = format!("{base}/missing");
            let temporary = format!("{base}/temporary");
            let requests = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let payload = fetch_badge_payload_with_retries(
                    badge_client(),
                    &[&missing, &temporary],
                    "test badges",
                    None,
                )
                .await;
                assert_eq!(payload.as_deref(), Some(IVR_BADGES));
                server.await.unwrap()
            })
            .await
            .unwrap();
            assert_eq!(requests.len(), 3);
            assert!(requests[0].starts_with("GET /missing "));
            assert!(requests[1].starts_with("GET /temporary "));
            assert!(requests[2].starts_with("GET /temporary "));
        }
    }

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
    fn bundled_badges_fill_missing_keys_without_overwriting_newer_cached_urls() {
        let map = BadgeMap::default();
        let first = r#"[{"set_id":"subscriber","versions":[{"id":"1","image_url_1x":"https://example.test/first.png"}]}]"#;
        let second = r#"[{"set_id":"subscriber","versions":[{"id":"1","image_url_1x":"https://example.test/second.png"}]}]"#;
        assert_eq!(
            seed_badge_response(first, "a", &map, parse_ivr_badge_response),
            Some(1)
        );
        assert_eq!(
            seed_badge_response(first, "b", &map, parse_ivr_badge_response),
            Some(1)
        );
        assert_eq!(
            seed_badge_response(first, "a", &map, parse_ivr_badge_response),
            Some(0)
        );
        assert_eq!(
            seed_badge_response(second, "a", &map, parse_ivr_badge_response),
            Some(0)
        );
        let map = map.read().unwrap();
        assert_eq!(
            resolve_badge_url(&map, "a", "subscriber", "1").as_deref(),
            Some("https://example.test/first.png")
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
            seed_badge_response(body, "a", &map, parse_ivr_badge_response),
            Some(1)
        );
        assert_eq!(
            seed_badge_response(body, "b", &map, parse_ivr_badge_response),
            Some(1)
        );
        assert_eq!(
            seed_badge_response("invalid", "a", &map, parse_ivr_badge_response),
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
            assert_eq!(seed_badge_response(empty, "room", &map, parse), Some(0));
            assert_eq!(seed_badge_response("not json", "room", &map, parse), None);
            assert_eq!(seed_badge_response("null", "room", &map, parse), None);
            assert_eq!(
                seed_badge_response(r#"{"error":"unavailable"}"#, "room", &map, parse),
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
        assert!(replace_badge_scope("a", &map, definitions.clone()));
        assert!(!replace_badge_scope("a", &map, definitions));
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
        assert!(replace_badge_scope("a", &map, empty));
        let snapshot = map.read().unwrap();
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.keys().all(|(scope, _, _)| scope != "a"));
    }
}

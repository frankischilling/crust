use std::collections::HashMap;

use serde::{Deserialize, Serialize};
/// Fix FFZ protocol-relative URLs ("//cdn.frankerfacez.com/...") by prepending "https:".
pub fn ffz_fix_url(url: String) -> String {
    if url.starts_with("//") {
        format!("https:{url}")
    } else {
        url
    }
}
// EmoteInfo: structure for emote metadata

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmoteInfo {
    /// Provider-scoped opaque id.
    pub id: String,
    /// Text code (e.g. "Kappa", "PogChamp").
    pub code: String,
    /// CDN URL for the 1x image.
    pub url_1x: String,
    /// CDN URL for the 2x image.
    pub url_2x: Option<String>,
    /// CDN URL for the 4x image.
    pub url_4x: Option<String>,
    /// Provider name: "twitch", "bttv", "ffz", "7tv".
    pub provider: String,
}

// EmoteProvider trait: implemented by each emote provider

/// Implement this for each emote provider (Twitch, BTTV, FFZ, 7TV).
#[async_trait::async_trait]
pub trait EmoteProvider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn load_global(&self) -> Vec<EmoteInfo>;
    async fn load_channel(&self, channel_id: &str) -> Vec<EmoteInfo>;
}

// BTTV: BetterTTV emote provider implementation

pub struct BttvProvider {
    client: reqwest::Client,
}

impl BttvProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[derive(Deserialize)]
struct BttvEmote {
    id: String,
    code: String,
    #[allow(dead_code)]
    #[serde(rename = "imageType")]
    image_type: String,
}

impl BttvEmote {
    fn url(&self, scale: &str) -> String {
        format!("https://cdn.betterttv.net/emote/{}/{}", self.id, scale)
    }

    fn into_info(self, provider: &str) -> EmoteInfo {
        EmoteInfo {
            url_1x: self.url("1x"),
            url_2x: Some(self.url("2x")),
            url_4x: Some(self.url("3x")),
            id: self.id,
            code: self.code,
            provider: provider.to_owned(),
        }
    }
}

#[async_trait::async_trait]
impl EmoteProvider for BttvProvider {
    fn name(&self) -> &'static str {
        "bttv"
    }

    async fn load_global(&self) -> Vec<EmoteInfo> {
        let url = "https://api.betterttv.net/3/cached/emotes/global";
        match self.client.get(url).send().await {
            Ok(resp) => resp
                .json::<Vec<BttvEmote>>()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|e| e.into_info("bttv"))
                .collect(),
            Err(e) => {
                tracing::warn!("BTTV global fetch failed: {e}");
                vec![]
            }
        }
    }

    async fn load_channel(&self, channel_id: &str) -> Vec<EmoteInfo> {
        let url = format!("https://api.betterttv.net/3/cached/users/twitch/{channel_id}");
        #[derive(Deserialize)]
        struct BttvChannel {
            #[serde(rename = "channelEmotes")]
            channel_emotes: Vec<BttvEmote>,
            #[serde(rename = "sharedEmotes")]
            shared_emotes: Vec<BttvEmote>,
        }
        match self.client.get(&url).send().await {
            Ok(resp) if !resp.status().is_success() => {
                tracing::debug!(
                    "BTTV channel returned HTTP {} for {channel_id}",
                    resp.status()
                );
                vec![]
            }
            Ok(resp) => match resp.json::<BttvChannel>().await {
                Ok(ch) => ch
                    .channel_emotes
                    .into_iter()
                    .chain(ch.shared_emotes)
                    .map(|e| e.into_info("bttv"))
                    .collect(),
                Err(e) => {
                    tracing::warn!("BTTV channel parse failed: {e}");
                    vec![]
                }
            },
            Err(e) => {
                tracing::warn!("BTTV channel fetch failed: {e}");
                vec![]
            }
        }
    }
}

// FFZ: FrankerFaceZ emote provider implementation

pub struct FfzProvider {
    client: reqwest::Client,
}

impl FfzProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl EmoteProvider for FfzProvider {
    fn name(&self) -> &'static str {
        "ffz"
    }

    async fn load_global(&self) -> Vec<EmoteInfo> {
        // FFZ global sets: https://api.frankerfacez.com/v1/set/global
        #[derive(Deserialize)]
        struct FfzEmote {
            id: u64,
            name: String,
            urls: HashMap<String, String>,
        }
        #[derive(Deserialize)]
        struct FfzSet {
            emoticons: Vec<FfzEmote>,
        }
        #[derive(Deserialize)]
        struct FfzGlobal {
            sets: HashMap<String, FfzSet>,
        }

        let url = "https://api.frankerfacez.com/v1/set/global";
        match self.client.get(url).send().await {
            Ok(resp) => match resp.json::<FfzGlobal>().await {
                Ok(g) => g
                    .sets
                    .into_values()
                    .flat_map(|s| s.emoticons)
                    .map(|e| EmoteInfo {
                        id: e.id.to_string(),
                        code: e.name.clone(),
                        url_1x: ffz_fix_url(e.urls.get("1").cloned().unwrap_or_default()),
                        url_2x: e.urls.get("2").cloned().map(ffz_fix_url),
                        url_4x: e.urls.get("4").cloned().map(ffz_fix_url),
                        provider: "ffz".to_owned(),
                    })
                    .collect(),
                Err(e) => {
                    tracing::warn!("FFZ global parse failed: {e}");
                    vec![]
                }
            },
            Err(e) => {
                tracing::warn!("FFZ global fetch failed: {e}");
                vec![]
            }
        }
    }

    async fn load_channel(&self, channel_id: &str) -> Vec<EmoteInfo> {
        // FFZ room by Twitch user-id: https://api.frankerfacez.com/v1/room/id/<id>
        #[derive(Deserialize)]
        struct FfzEmote {
            id: u64,
            name: String,
            urls: HashMap<String, String>,
        }
        #[derive(Deserialize)]
        struct FfzSet {
            emoticons: Vec<FfzEmote>,
        }
        #[derive(Deserialize)]
        struct FfzRoom {
            sets: HashMap<String, FfzSet>,
        }

        let url = format!("https://api.frankerfacez.com/v1/room/id/{channel_id}");
        match self.client.get(&url).send().await {
            Ok(resp) if !resp.status().is_success() => {
                tracing::debug!(
                    "FFZ channel returned HTTP {} for {channel_id}",
                    resp.status()
                );
                vec![]
            }
            Ok(resp) => match resp.json::<FfzRoom>().await {
                Ok(r) => r
                    .sets
                    .into_values()
                    .flat_map(|s| s.emoticons)
                    .map(|e| EmoteInfo {
                        id: e.id.to_string(),
                        code: e.name.clone(),
                        url_1x: ffz_fix_url(e.urls.get("1").cloned().unwrap_or_default()),
                        url_2x: e.urls.get("2").cloned().map(ffz_fix_url),
                        url_4x: e.urls.get("4").cloned().map(ffz_fix_url),
                        provider: "ffz".to_owned(),
                    })
                    .collect(),
                Err(e) => {
                    tracing::warn!("FFZ channel parse failed: {e}");
                    vec![]
                }
            },
            Err(e) => {
                tracing::warn!("FFZ channel fetch failed: {e}");
                vec![]
            }
        }
    }
}

// SevenTV: 7TV emote provider implementation

pub struct SevenTvProvider {
    client: reqwest::Client,
}

impl SevenTvProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl EmoteProvider for SevenTvProvider {
    fn name(&self) -> &'static str {
        "7tv"
    }

    async fn load_global(&self) -> Vec<EmoteInfo> {
        let url = "https://7tv.io/v3/emote-sets/global";
        self.fetch_emote_set(url).await
    }

    async fn load_channel(&self, channel_id: &str) -> Vec<EmoteInfo> {
        let url = format!("https://7tv.io/v3/users/twitch/{channel_id}");
        self.fetch_user_emote_set(&url, "7tv channel").await
    }
}

/// Twitch native global emote provider.
///
/// Twitch global emotes are extremely stable and don't require authentication
/// to look up by ID on the CDN. We hardcode the well-known set here so the
/// "Twitch" tab in the emote picker is never empty.
pub struct TwitchGlobalProvider;

impl TwitchGlobalProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl EmoteProvider for TwitchGlobalProvider {
    fn name(&self) -> &'static str {
        "twitch"
    }

    async fn load_global(&self) -> Vec<EmoteInfo> {
        twitch_global_emotes()
    }

    async fn load_channel(&self, _channel_id: &str) -> Vec<EmoteInfo> {
        vec![]
    }
}

fn twitch_cdn(id: &str, scale: &str) -> String {
    // Use `static` format to guarantee PNG responses.  `default` may return
    // WebP which the image crate can't decode without the webp feature.
    format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/static/dark/{scale}")
}

fn tw(id: &str, code: &str) -> EmoteInfo {
    EmoteInfo {
        id: id.to_owned(),
        code: code.to_owned(),
        url_1x: twitch_cdn(id, "1.0"),
        url_2x: Some(twitch_cdn(id, "2.0")),
        url_4x: Some(twitch_cdn(id, "3.0")),
        provider: "twitch".to_owned(),
    }
}

/// Returns the well-known Twitch global emote set.
///
/// This intentionally keeps the startup list small. The full Twitch catalog
/// is very large and was blowing the default worker stack during bootstrap on
/// this build. Additional emotes can still be loaded later through channel
/// emote refreshes.
fn twitch_global_emotes() -> Vec<EmoteInfo> {
    vec![
        tw("25", "Kappa"),
        tw("425618", "LUL"),
        tw("305954156", "PogChamp"),
        tw("354", "4Head"),
        tw("35", "KappaHD"),
        tw("160401", "PunOko"),
        tw("106293", "VoteYea"),
        tw("106294", "VoteNay"),
        tw("52", "SMOrc"),
        tw("34", "SwiftRage"),
        tw("41", "Kreygasm"),
        tw("245", "ResidentSleeper"),
        tw("81274", "VoHiYo"),
        tw("28087", "WutFace"),
        tw("160395", "Kappu"),
        tw("81273", "KomodoHype"),
        tw("160400", "KonCha"),
        tw("160403", "TearGlove"),
        tw("160404", "TehePelo"),
        tw("1902", "Keepo"),
    ]
}

/// Kick native emote provider.
pub struct KickProvider {
    client: reqwest::Client,
}

impl KickProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl EmoteProvider for KickProvider {
    fn name(&self) -> &'static str {
        "kick"
    }

    async fn load_global(&self) -> Vec<EmoteInfo> {
        // Kick global emotes are not loaded yet.
        vec![]
    }

    async fn load_channel(&self, channel_id: &str) -> Vec<EmoteInfo> {
        let url = format!("https://kick.com/emotes/{channel_id}");
        match self
            .client
            .get(&url)
            .header(reqwest::header::USER_AGENT, "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
        {
            Ok(resp) if !resp.status().is_success() => {
                tracing::debug!(
                    "Kick channel emotes returned HTTP {} for {channel_id}",
                    resp.status()
                );
                vec![]
            }
            Ok(resp) => match resp.json::<serde_json::Value>().await {
                Ok(v) => parse_kick_emotes(v),
                Err(e) => {
                    tracing::warn!("Kick channel emotes parse failed: {e}");
                    vec![]
                }
            },
            Err(e) => {
                tracing::warn!("Kick channel emotes fetch failed: {e}");
                vec![]
            }
        }
    }
}

#[derive(Deserialize)]
struct EmoteSetResp {
    emotes: Vec<EmoteSetEntry>,
}

#[derive(Deserialize)]
struct EmoteSetEntry {
    id: String,
    name: String,
    data: Option<EmoteData>,
}

#[derive(Deserialize)]
struct EmoteData {
    host: EmoteHost,
}

#[derive(Deserialize)]
struct EmoteHost {
    url: String,
    files: Vec<EmoteFile>,
}

#[derive(Deserialize)]
struct EmoteFile {
    name: String,
    #[serde(default)]
    width: Option<u32>,
}

fn seven_tv_file_scale(file_name: &str) -> Option<u8> {
    let name = file_name
        .split('.')
        .next()
        .unwrap_or(file_name)
        .to_ascii_lowercase();
    if !name.ends_with('x') {
        return None;
    }
    let digits = &name[..name.len().saturating_sub(1)];
    digits.parse::<u8>().ok()
}

fn choose_7tv_file_by_scale<'a>(
    files: &'a [EmoteFile],
    preferences: &[u8],
) -> Option<&'a EmoteFile> {
    for preferred in preferences {
        if let Some(file) = files
            .iter()
            .find(|f| seven_tv_file_scale(&f.name) == Some(*preferred))
        {
            return Some(file);
        }
    }
    None
}

fn choose_7tv_smallest_file(files: &[EmoteFile]) -> Option<&EmoteFile> {
    files
        .iter()
        .min_by_key(|f| f.width.unwrap_or(u32::MAX))
        .or_else(|| files.first())
}

impl SevenTvProvider {
    async fn fetch_user_emote_set(&self, url: &str, label: &str) -> Vec<EmoteInfo> {
        #[derive(Deserialize)]
        struct UserResp {
            emote_set: Option<EmoteSetResp>,
        }
        match self.client.get(url).send().await {
            Ok(r) if !r.status().is_success() => {
                tracing::debug!("{label} returned HTTP {}", r.status());
                vec![]
            }
            Ok(r) => match r.json::<UserResp>().await {
                Ok(u) => {
                    if let Some(set) = u.emote_set {
                        Self::parse_emote_set(set)
                    } else {
                        vec![]
                    }
                }
                Err(e) => {
                    tracing::warn!("{label} parse failed: {e}");
                    vec![]
                }
            },
            Err(e) => {
                tracing::warn!("{label} fetch failed: {e}");
                vec![]
            }
        }
    }

    async fn fetch_emote_set(&self, url: &str) -> Vec<EmoteInfo> {
        match self.client.get(url).send().await {
            Ok(r) => match r.json::<EmoteSetResp>().await {
                Ok(set) => Self::parse_emote_set(set),
                Err(e) => {
                    tracing::warn!("7tv parse failed: {e}");
                    vec![]
                }
            },
            Err(e) => {
                tracing::warn!("7tv fetch failed: {e}");
                vec![]
            }
        }
    }

    /// Load 7TV emotes for a Kick user-id.
    pub async fn load_kick_channel(&self, kick_user_id: &str) -> Vec<EmoteInfo> {
        let url = format!("https://7tv.io/v3/users/kick/{kick_user_id}");
        self.fetch_user_emote_set(&url, "7tv kick channel").await
    }

    fn parse_emote_set(set: EmoteSetResp) -> Vec<EmoteInfo> {
        set.emotes
            .into_iter()
            .filter_map(|e| {
                let data = e.data?;
                let base = format!("https:{}", data.host.url.trim_end_matches('/'));
                let files = data.host.files;

                let file_1x = choose_7tv_file_by_scale(&files, &[1])
                    .or_else(|| choose_7tv_smallest_file(&files))
                    .map(|f| format!("{base}/{}", f.name));
                let file_2x = choose_7tv_file_by_scale(&files, &[2, 3, 4])
                    .map(|f| format!("{base}/{}", f.name));
                // Prefer true 4x assets for HD previews; fall back to 3x/2x.
                let file_4x = choose_7tv_file_by_scale(&files, &[4, 3, 2])
                    .map(|f| format!("{base}/{}", f.name));
                Some(EmoteInfo {
                    id: e.id,
                    code: e.name,
                    url_1x: file_1x?,
                    url_2x: file_2x,
                    url_4x: file_4x,
                    provider: "7tv".to_owned(),
                })
            })
            .collect()
    }
}

fn parse_kick_emotes(v: serde_json::Value) -> Vec<EmoteInfo> {
    let items_opt = v
        .get("emotes")
        .and_then(|e| e.as_array())
        .or_else(|| v.as_array());
    let Some(items) = items_opt else {
        return Vec::new();
    };

    let mut out: Vec<EmoteInfo> = Vec::new();
    for item in items {
        if let Some(e) = parse_kick_emote_item(item) {
            out.push(e);
        }
    }
    out
}

fn parse_kick_emote_item(item: &serde_json::Value) -> Option<EmoteInfo> {
    let code = first_str(item, &["name", "code", "slug", "keyword"])?;
    let id = first_str(item, &["id", "emote_id", "uuid"]).unwrap_or(code.clone());

    let urls = extract_kick_emote_urls(item);
    let url_1x = urls.0?;

    Some(EmoteInfo {
        id,
        code,
        url_1x,
        url_2x: urls.1,
        url_4x: urls.2,
        provider: "kick".to_owned(),
    })
}

fn extract_kick_emote_urls(
    item: &serde_json::Value,
) -> (Option<String>, Option<String>, Option<String>) {
    if let Some(urls_obj) = item.get("urls").and_then(|u| u.as_object()) {
        let u1 =
            first_obj_str(urls_obj, &["1x", "1", "small", "url"]).map(|u| normalize_kick_url(&u));
        let u2 = first_obj_str(urls_obj, &["2x", "2", "medium"]).map(|u| normalize_kick_url(&u));
        let u4 =
            first_obj_str(urls_obj, &["4x", "3x", "3", "large"]).map(|u| normalize_kick_url(&u));
        if u1.is_some() {
            return (u1, u2, u4);
        }
    }

    for key in ["image", "emote"] {
        if let Some(obj) = item.get(key).and_then(|v| v.as_object()) {
            let u1 =
                first_obj_str(obj, &["url", "src", "1x", "small"]).map(|u| normalize_kick_url(&u));
            let u2 = first_obj_str(obj, &["2x", "medium"]).map(|u| normalize_kick_url(&u));
            let u4 = first_obj_str(obj, &["4x", "3x", "large"]).map(|u| normalize_kick_url(&u));
            if u1.is_some() {
                return (u1, u2, u4);
            }
        }
    }

    let u1 = first_str(item, &["url", "src", "image_url"]).map(|u| normalize_kick_url(&u));
    (u1, None, None)
}

fn first_str(item: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(s) = item.get(*key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(s.to_owned());
            }
        }
        if let Some(n) = item.get(*key).and_then(|v| v.as_u64()) {
            return Some(n.to_string());
        }
    }
    None
}

fn first_obj_str(
    obj: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<String> {
    for key in keys {
        if let Some(s) = obj.get(*key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(s.to_owned());
            }
        }
    }
    None
}

fn normalize_kick_url(url: &str) -> String {
    if url.starts_with("//") {
        format!("https:{url}")
    } else if url.starts_with('/') {
        format!("https://kick.com{url}")
    } else {
        url.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_7tv_set(value: serde_json::Value) -> Vec<EmoteInfo> {
        let set: EmoteSetResp = serde_json::from_value(value).expect("valid emote set json");
        SevenTvProvider::parse_emote_set(set)
    }

    #[test]
    fn seven_tv_prefers_true_4x_file_for_hd_url() {
        let emotes = parse_7tv_set(serde_json::json!({
            "emotes": [{
                "id": "1",
                "name": "OMEGALUL",
                "data": {
                    "host": {
                        "url": "//cdn.7tv.app/emote/abc",
                        "files": [
                            { "name": "1x.webp", "width": 32 },
                            { "name": "3x.webp", "width": 96 },
                            { "name": "4x.webp", "width": 128 },
                            { "name": "2x.webp", "width": 64 }
                        ]
                    }
                }
            }]
        }));

        assert_eq!(emotes.len(), 1);
        let e = &emotes[0];
        assert_eq!(e.url_1x, "https://cdn.7tv.app/emote/abc/1x.webp");
        assert_eq!(
            e.url_2x.as_deref(),
            Some("https://cdn.7tv.app/emote/abc/2x.webp")
        );
        assert_eq!(
            e.url_4x.as_deref(),
            Some("https://cdn.7tv.app/emote/abc/4x.webp")
        );
    }

    #[test]
    fn seven_tv_falls_back_to_3x_when_4x_missing() {
        let emotes = parse_7tv_set(serde_json::json!({
            "emotes": [{
                "id": "1",
                "name": "forsenE",
                "data": {
                    "host": {
                        "url": "//cdn.7tv.app/emote/def",
                        "files": [
                            { "name": "1x.webp", "width": 32 },
                            { "name": "2x.webp", "width": 64 },
                            { "name": "3x.webp", "width": 96 }
                        ]
                    }
                }
            }]
        }));

        assert_eq!(emotes.len(), 1);
        assert_eq!(
            emotes[0].url_4x.as_deref(),
            Some("https://cdn.7tv.app/emote/def/3x.webp")
        );
    }

    #[test]
    fn seven_tv_uses_smallest_width_when_scale_names_are_unavailable() {
        let emotes = parse_7tv_set(serde_json::json!({
            "emotes": [{
                "id": "1",
                "name": "widepeepoHappy",
                "data": {
                    "host": {
                        "url": "//cdn.7tv.app/emote/ghi",
                        "files": [
                            { "name": "a.webp", "width": 128 },
                            { "name": "b.webp", "width": 48 },
                            { "name": "c.webp", "width": 64 }
                        ]
                    }
                }
            }]
        }));

        assert_eq!(emotes.len(), 1);
        assert_eq!(emotes[0].url_1x, "https://cdn.7tv.app/emote/ghi/b.webp");
    }
}

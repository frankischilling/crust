use std::collections::HashMap;

use serde::{Deserialize, Serialize};

fn provider_client() -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("create emote provider HTTP client")
        })
        .clone()
}

/// Bound metadata responses before parsing JSON, including chunked responses.
pub async fn read_bounded_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Option<T> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len().checked_add(chunk.len())? > max_bytes {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).ok()
}

async fn fetch_channel_json<T: serde::de::DeserializeOwned>(
    request: reqwest::RequestBuilder,
) -> Option<Option<T>> {
    let response = request.send().await.ok()?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Some(None);
    }
    let response = response.error_for_status().ok()?;
    read_bounded_json(response, 8 * 1024 * 1024).await.map(Some)
}
/// Fix FFZ protocol-relative URLs ("//cdn.frankerfacez.com/...") by prepending "https:".
pub fn ffz_fix_url(url: String) -> String {
    if url.starts_with("//") {
        format!("https:{url}")
    } else {
        url
    }
}
// EmoteInfo: structure for emote metadata

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmoteInfo {
    /// Provider-scoped opaque id.
    pub id: String,
    /// Text code (e.g. "Kappa", "PogChamp").
    pub code: String,
    /// CDN URL used at chat size; the provider chooses the source resolution.
    pub url_1x: String,
    /// CDN URL for the 2x image.
    pub url_2x: Option<String>,
    /// CDN URL for a larger tooltip preview.
    pub url_4x: Option<String>,
    /// Provider name: "twitch", "bttv", "ffz", "7tv".
    pub provider: String,
    /// Draw over the preceding emote when enabled in this active emote set.
    #[serde(default)]
    pub zero_width: bool,
}

// EmoteProvider trait: implemented by each emote provider

/// Implement this for each emote provider (Twitch, BTTV, FFZ, 7TV).
#[async_trait::async_trait]
pub trait EmoteProvider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn load_global(&self) -> Vec<EmoteInfo>;
    /// `None` is a failed refresh; `Some([])` is a successfully empty set.
    async fn load_channel(&self, channel_id: &str) -> Option<Vec<EmoteInfo>>;
}

// BTTV: BetterTTV emote provider implementation

pub struct BttvProvider {
    client: reqwest::Client,
}

impl BttvProvider {
    pub fn new() -> Self {
        Self {
            client: provider_client(),
        }
    }

    pub async fn try_load_global(&self) -> Option<Vec<EmoteInfo>> {
        let emotes = fetch_channel_json::<Vec<BttvEmote>>(
            self.client
                .get("https://api.betterttv.net/3/cached/emotes/global"),
        )
        .await??;
        Some(
            emotes
                .into_iter()
                .map(|emote| emote.into_info(true))
                .collect(),
        )
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

    fn into_info(self, global: bool) -> EmoteInfo {
        let zero_width = global
            && matches!(
                self.code.as_str(),
                "SoSnowy"
                    | "IceCold"
                    | "SantaHat"
                    | "TopHat"
                    | "ReinDeer"
                    | "CandyCane"
                    | "cvMask"
                    | "cvHazmat"
            );
        EmoteInfo {
            url_1x: self.url("1x"),
            url_2x: Some(self.url("2x")),
            url_4x: Some(self.url("3x")),
            id: self.id,
            code: self.code,
            provider: "bttv".to_owned(),
            zero_width,
        }
    }
}

#[async_trait::async_trait]
impl EmoteProvider for BttvProvider {
    fn name(&self) -> &'static str {
        "bttv"
    }

    async fn load_global(&self) -> Vec<EmoteInfo> {
        self.try_load_global().await.unwrap_or_default()
    }

    async fn load_channel(&self, channel_id: &str) -> Option<Vec<EmoteInfo>> {
        let url = format!("https://api.betterttv.net/3/cached/users/twitch/{channel_id}");
        #[derive(Deserialize)]
        struct BttvChannel {
            #[serde(rename = "channelEmotes")]
            channel_emotes: Vec<BttvEmote>,
            #[serde(rename = "sharedEmotes")]
            shared_emotes: Vec<BttvEmote>,
        }
        let Some(ch) = fetch_channel_json::<BttvChannel>(self.client.get(&url)).await? else {
            return Some(Vec::new());
        };
        Some(
            ch.channel_emotes
                .into_iter()
                .chain(ch.shared_emotes)
                .map(|emote| emote.into_info(false))
                .collect(),
        )
    }
}

// FFZ: FrankerFaceZ emote provider implementation

pub struct FfzProvider {
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct FfzEmote {
    id: u64,
    name: String,
    urls: HashMap<String, String>,
    #[serde(default)]
    animated: Option<HashMap<String, String>>,
}

impl FfzEmote {
    fn into_info(self) -> Option<EmoteInfo> {
        let usable_url = |urls: &HashMap<String, String>, scale: &str| {
            urls.get(scale)
                .filter(|url| !url.trim().is_empty())
                .cloned()
                .map(ffz_fix_url)
        };
        let urls = self
            .animated
            .as_ref()
            .filter(|urls| {
                ["1", "2", "4"]
                    .into_iter()
                    .any(|scale| urls.get(scale).is_some_and(|url| !url.trim().is_empty()))
            })
            .unwrap_or(&self.urls);
        // Keep every scale in the selected format so previews also animate.
        let url_1x = ["1", "2", "4"]
            .into_iter()
            .find_map(|scale| usable_url(urls, scale))?;
        Some(EmoteInfo {
            id: self.id.to_string(),
            code: self.name,
            url_1x,
            url_2x: usable_url(urls, "2"),
            url_4x: usable_url(urls, "4"),
            provider: "ffz".into(),
            zero_width: false,
        })
    }
}

#[derive(Deserialize)]
struct FfzSet {
    emoticons: Vec<FfzEmote>,
}

#[derive(Deserialize)]
struct FfzGlobal {
    default_sets: Vec<u64>,
    sets: HashMap<String, FfzSet>,
}

impl FfzGlobal {
    fn into_emotes(mut self) -> Vec<EmoteInfo> {
        self.default_sets
            .into_iter()
            .filter_map(|id| self.sets.remove(&id.to_string()))
            .flat_map(|set| set.emoticons)
            .filter_map(FfzEmote::into_info)
            .collect()
    }
}

#[derive(Deserialize)]
struct FfzRoom {
    sets: HashMap<String, FfzSet>,
}

impl FfzRoom {
    fn into_emotes(self) -> Vec<EmoteInfo> {
        self.sets
            .into_values()
            .flat_map(|set| set.emoticons)
            .filter_map(FfzEmote::into_info)
            .collect()
    }
}

impl FfzProvider {
    pub fn new() -> Self {
        Self {
            client: provider_client(),
        }
    }

    pub async fn try_load_global(&self) -> Option<Vec<EmoteInfo>> {
        let global = fetch_channel_json::<FfzGlobal>(
            self.client
                .get("https://api.frankerfacez.com/v1/set/global"),
        )
        .await??;
        Some(global.into_emotes())
    }
}

#[async_trait::async_trait]
impl EmoteProvider for FfzProvider {
    fn name(&self) -> &'static str {
        "ffz"
    }

    async fn load_global(&self) -> Vec<EmoteInfo> {
        self.try_load_global().await.unwrap_or_default()
    }

    async fn load_channel(&self, channel_id: &str) -> Option<Vec<EmoteInfo>> {
        // FFZ room by Twitch user-id: https://api.frankerfacez.com/v1/room/id/<id>
        let url = format!("https://api.frankerfacez.com/v1/room/id/{channel_id}");
        let Some(room) = fetch_channel_json::<FfzRoom>(self.client.get(&url)).await? else {
            return Some(Vec::new());
        };
        Some(room.into_emotes())
    }
}

// SevenTV: 7TV emote provider implementation

pub struct SevenTvProvider {
    client: reqwest::Client,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SevenTvChannel {
    pub emotes: Vec<EmoteInfo>,
    pub user_id: Option<String>,
    pub set_id: Option<String>,
    pub connection_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SevenTvSet {
    pub id: String,
    pub emotes: Vec<EmoteInfo>,
}

impl SevenTvProvider {
    pub fn new() -> Self {
        Self {
            client: provider_client(),
        }
    }

    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl EmoteProvider for SevenTvProvider {
    fn name(&self) -> &'static str {
        "7tv"
    }

    async fn load_global(&self) -> Vec<EmoteInfo> {
        self.load_global_state()
            .await
            .map(|set| set.emotes)
            .unwrap_or_default()
    }

    async fn load_channel(&self, channel_id: &str) -> Option<Vec<EmoteInfo>> {
        self.load_channel_state("TWITCH", channel_id)
            .await
            .map(|state| state.emotes)
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

    async fn load_channel(&self, _channel_id: &str) -> Option<Vec<EmoteInfo>> {
        Some(vec![])
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
        zero_width: false,
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
            client: provider_client(),
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

    async fn load_channel(&self, channel_id: &str) -> Option<Vec<EmoteInfo>> {
        let url = format!("https://kick.com/emotes/{channel_id}");
        let request = self
            .client
            .get(&url)
            .header(reqwest::header::USER_AGENT, "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
            .header(reqwest::header::ACCEPT, "application/json");
        Some(
            fetch_channel_json::<serde_json::Value>(request)
                .await?
                .map(parse_kick_emotes)
                .unwrap_or_default(),
        )
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
    #[serde(default)]
    flags: u64,
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

fn valid_7tv_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Parse the complete active-emote shape used by REST and live set updates.
pub fn parse_7tv_emote(value: serde_json::Value) -> Option<EmoteInfo> {
    parse_7tv_emote_entry(serde_json::from_value(value).ok()?)
}

/// Empty REST sets can omit `emotes`; their set ID distinguishes them from invalid JSON.
pub fn parse_7tv_emote_set(value: serde_json::Value) -> Option<Vec<EmoteInfo>> {
    let object = value.as_object()?;
    if !object.contains_key("emotes") {
        let id = object.get("id")?.as_str()?;
        if !valid_7tv_id(id)
            || object
                .get("emote_count")
                .is_some_and(|count| count.as_u64() != Some(0))
        {
            return None;
        }
        return Some(Vec::new());
    }
    let set: EmoteSetResp = serde_json::from_value(value).ok()?;
    Some(SevenTvProvider::parse_emote_set(set))
}

fn parse_7tv_emote_entry(emote: EmoteSetEntry) -> Option<EmoteInfo> {
    if !valid_7tv_id(&emote.id)
        || emote.name.is_empty()
        || emote.name.len() > 128
        || emote
            .name
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return None;
    }
    let data = emote.data?;
    let host = data.host.url.trim_end_matches('/');
    let base = if host.starts_with("//") {
        format!("https:{host}")
    } else {
        host.to_owned()
    };
    let url = reqwest::Url::parse(&base).ok()?;
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let files = data.host.files;
    // Keep one supported format across sizes so a static variant cannot replace
    // an animated WebP merely because it has a closer scale.
    let format = files
        .iter()
        .filter_map(|file| seven_tv_format_priority(&file.name))
        .min()?;
    let files: Vec<_> = files
        .into_iter()
        .filter(|file| seven_tv_format_priority(&file.name) == Some(format))
        .collect();
    // A 2x asset stays sharp in chat at common display scales. Larger previews
    // have their own URL and load only when the user opens a tooltip.
    let file_1x = choose_7tv_file_by_scale(&files, &[2, 3, 4, 1])
        .or_else(|| choose_7tv_smallest_file(&files))
        .map(|file| format!("{base}/{}", file.name));
    let file_2x =
        choose_7tv_file_by_scale(&files, &[2, 3, 4]).map(|file| format!("{base}/{}", file.name));
    let file_4x =
        choose_7tv_file_by_scale(&files, &[4, 3, 2]).map(|file| format!("{base}/{}", file.name));
    Some(EmoteInfo {
        id: emote.id,
        code: emote.name,
        url_1x: file_1x?,
        url_2x: file_2x,
        url_4x: file_4x,
        provider: "7tv".to_owned(),
        zero_width: emote.flags & 1 != 0,
    })
}

fn parse_7tv_channel(
    value: serde_json::Value,
    platform: &str,
    room_id: &str,
) -> Option<SevenTvChannel> {
    let object = value.as_object()?;
    let user = object.get("user").filter(|user| user.is_object());
    let user_id = user
        .and_then(|user| user.get("id"))
        .and_then(|id| id.as_str())
        .filter(|id| valid_7tv_id(id))
        .map(str::to_owned);
    let connection_index = user
        .and_then(|user| user.get("connections"))
        .and_then(|connections| connections.as_array())
        .and_then(|connections| {
            connections.iter().position(|connection| {
                connection.get("id").and_then(|id| id.as_str()) == Some(room_id)
                    && connection
                        .get("platform")
                        .and_then(|platform| platform.as_str())
                        .is_some_and(|value| value.eq_ignore_ascii_case(platform))
            })
        });
    let outer_set_id = match object.get("emote_set_id") {
        Some(serde_json::Value::Null) | None => None,
        Some(value) => Some(value.as_str().filter(|id| valid_7tv_id(id))?.to_owned()),
    };
    let set = object.get("emote_set");
    if let (Some(outer), Some(nested)) = (
        outer_set_id.as_deref(),
        set.and_then(|set| set.get("id")).and_then(|id| id.as_str()),
    ) {
        if outer != nested {
            return None;
        }
    }
    let set_id = outer_set_id.or_else(|| {
        set.and_then(|set| set.get("id"))
            .and_then(|id| id.as_str())
            .filter(|id| valid_7tv_id(id))
            .map(str::to_owned)
    });
    let emotes = match set {
        Some(serde_json::Value::Null) => {
            if set_id.is_some() {
                return None;
            }
            Vec::new()
        }
        Some(set) => parse_7tv_emote_set(set.clone())?,
        None => {
            let known_connection = object.get("id").and_then(|id| id.as_str()) == Some(room_id)
                && object
                    .get("platform")
                    .and_then(|platform| platform.as_str())
                    .is_some_and(|value| value.eq_ignore_ascii_case(platform));
            if set_id.is_some() || (!known_connection && connection_index.is_none()) {
                return None;
            }
            Vec::new()
        }
    };
    Some(SevenTvChannel {
        emotes,
        user_id,
        set_id,
        connection_index,
    })
}

fn seven_tv_file_scale(file_name: &str) -> Option<u8> {
    let name = file_name.split('.').next()?;
    let digits = name.strip_suffix('x').or_else(|| name.strip_suffix('X'))?;
    digits.parse::<u8>().ok()
}

fn choose_7tv_file_by_scale<'a>(
    files: &'a [EmoteFile],
    preferences: &[u8],
) -> Option<&'a EmoteFile> {
    for preferred in preferences {
        if let Some(file) = files
            .iter()
            .find(|file| seven_tv_file_scale(&file.name) == Some(*preferred))
        {
            return Some(file);
        }
    }
    None
}

fn choose_7tv_smallest_file(files: &[EmoteFile]) -> Option<&EmoteFile> {
    files
        .iter()
        .min_by_key(|file| file.width.unwrap_or(u32::MAX))
}

fn seven_tv_format_priority(file_name: &str) -> Option<u8> {
    let extension = file_name.rsplit('.').next()?;
    [("webp", 0), ("gif", 1), ("png", 2), ("jpg", 3), ("jpeg", 3)]
        .into_iter()
        .find_map(|(supported, priority)| {
            extension
                .eq_ignore_ascii_case(supported)
                .then_some(priority)
        })
}

pub fn personal_emote_set_id(user: &serde_json::Value) -> Option<&str> {
    user.get("user")
        .unwrap_or(user)
        .get("emote_sets")?
        .as_array()?
        .iter()
        .find_map(|set| {
            let flags = set.get("flags")?.as_u64()?;
            let id = set.get("id")?.as_str()?;
            (flags & (1 << 2) != 0
                && !id.is_empty()
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'))
            .then_some(id)
        })
}

impl SevenTvProvider {
    pub async fn load_global_state(&self) -> Option<SevenTvSet> {
        self.fetch_global_state("https://7tv.io/v3/emote-sets/global")
            .await
    }

    async fn fetch_global_state(&self, url: &str) -> Option<SevenTvSet> {
        let value = fetch_channel_json::<serde_json::Value>(self.client.get(url)).await??;
        let id = value.get("id")?.as_str()?;
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return None;
        }
        let id = id.to_owned();
        Some(SevenTvSet {
            id,
            emotes: parse_7tv_emote_set(value)?,
        })
    }

    /// Reuse the user lookup already made for cosmetics; only a personal set needs another request.
    pub async fn load_personal_from_user(
        &self,
        user: &serde_json::Value,
    ) -> Option<Vec<EmoteInfo>> {
        let Some(set_id) = personal_emote_set_id(user) else {
            return Some(Vec::new());
        };
        let response = self
            .client
            .get(format!("https://7tv.io/v3/emote-sets/{set_id}"))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        let set = read_bounded_json::<serde_json::Value>(response, 8 * 1024 * 1024).await?;
        parse_7tv_emote_set(set)
    }

    pub async fn load_channel_state(
        &self,
        platform: &str,
        room_id: &str,
    ) -> Option<SevenTvChannel> {
        let path = match platform {
            "TWITCH" => "twitch",
            "KICK" => "kick",
            _ => return None,
        };
        if room_id.is_empty()
            || room_id.len() > 32
            || !room_id.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let url = format!("https://7tv.io/v3/users/{path}/{room_id}");
        self.fetch_channel_state(&url, platform, room_id).await
    }

    async fn fetch_channel_state(
        &self,
        url: &str,
        platform: &str,
        room_id: &str,
    ) -> Option<SevenTvChannel> {
        match fetch_channel_json::<serde_json::Value>(self.client.get(url)).await? {
            Some(value) => parse_7tv_channel(value, platform, room_id),
            None => Some(SevenTvChannel::default()),
        }
    }

    /// Load 7TV emotes for a Kick user-id.
    pub async fn load_kick_channel(&self, kick_user_id: &str) -> Option<Vec<EmoteInfo>> {
        self.load_channel_state("KICK", kick_user_id)
            .await
            .map(|state| state.emotes)
    }

    fn parse_emote_set(set: EmoteSetResp) -> Vec<EmoteInfo> {
        set.emotes
            .into_iter()
            .filter_map(parse_7tv_emote_entry)
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
        zero_width: false,
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

    #[test]
    fn ffz_global_catalog_uses_only_default_sets() {
        let global: FfzGlobal = serde_json::from_value(serde_json::json!({
            "default_sets": [3, 3],
            "sets": {
                "3": {"emoticons": [{"id": 1, "name": "Default", "urls": {"1": "//cdn.example/default.png"}}]},
                "9": {"emoticons": [{"id": 2, "name": "Private", "urls": {"1": "//cdn.example/private.png"}}]}
            }
        })).unwrap();
        let emotes = global.into_emotes();
        assert_eq!(emotes.len(), 1);
        assert_eq!(emotes[0].code, "Default");
        assert_eq!(emotes[0].url_1x, "https://cdn.example/default.png");
        assert!(!emotes[0].zero_width);

        let empty: FfzGlobal = serde_json::from_value(serde_json::json!({
            "default_sets": [], "sets": {}
        }))
        .unwrap();
        assert!(empty.into_emotes().is_empty());
        assert!(serde_json::from_value::<FfzGlobal>(serde_json::json!({"sets": {}})).is_err());
    }

    #[test]
    fn ffz_channel_and_global_emotes_keep_animated_assets_at_every_scale() {
        let value = serde_json::json!({
            "default_sets": [3],
            "sets": {"3": {"emoticons": [{
                "id": 1, "name": "Animated",
                "urls": {"1": "//cdn.example/static.png", "2": "//cdn.example/static2.png", "4": "//cdn.example/static4.png"},
                "animated": {"1": "//cdn.example/animated.webp", "2": "https://cdn.example/animated2.webp", "4": "//cdn.example/animated4.webp"}
            }]}}
        });
        let global = serde_json::from_value::<FfzGlobal>(value.clone())
            .unwrap()
            .into_emotes();
        let channel = serde_json::from_value::<FfzRoom>(value)
            .unwrap()
            .into_emotes();
        assert_eq!(global, channel);
        assert_eq!(global[0].url_1x, "https://cdn.example/animated.webp");
        assert_eq!(
            global[0].url_2x.as_deref(),
            Some("https://cdn.example/animated2.webp")
        );
        assert_eq!(
            global[0].url_4x.as_deref(),
            Some("https://cdn.example/animated4.webp")
        );
    }

    #[test]
    fn ffz_assets_fall_back_without_losing_animation_or_emitting_empty_urls() {
        for (animated, expected) in [
            (serde_json::Value::Null, "https://cdn.example/static.png"),
            (serde_json::json!({}), "https://cdn.example/static.png"),
            (
                serde_json::json!({"1": "", "4": " "}),
                "https://cdn.example/static.png",
            ),
            (
                serde_json::json!({"2": "//cdn.example/animated2.webp"}),
                "https://cdn.example/animated2.webp",
            ),
        ] {
            let emote = serde_json::from_value::<FfzEmote>(serde_json::json!({
                "id": 1, "name": "Fallback", "urls": {"1": "//cdn.example/static.png"}, "animated": animated
            })).unwrap().into_info().unwrap();
            assert_eq!(emote.url_1x, expected);
            assert!(emote.url_4x.is_none());
        }
        let emote = serde_json::from_value::<FfzEmote>(serde_json::json!({
            "id": 1, "name": "LargeOnly", "urls": {"4": "//cdn.example/large.png"}
        }))
        .unwrap()
        .into_info()
        .unwrap();
        assert_eq!(emote.url_1x, "https://cdn.example/large.png");
        let missing = serde_json::from_value::<FfzEmote>(serde_json::json!({
            "id": 1, "name": "Missing", "urls": {"1": ""}, "animated": {"2": " "}
        }))
        .unwrap();
        assert!(missing.into_info().is_none());
    }

    fn active_emote(host: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "emote-id", "name": "Hello",
            "data": { "host": { "url": host, "files": [
                { "name": "1x.webp", "width": 32 },
                { "name": "2x.webp", "width": 64 },
                { "name": "4x.webp", "width": 128 }
            ]}}
        })
    }

    #[test]
    fn channel_metadata_tracks_provider_user_set_and_matching_connection() {
        let response = serde_json::json!({
            "id": "42", "platform": "TWITCH", "emote_set_id": "nested-set",
            "emote_set": { "id": "nested-set", "emotes": [active_emote("//cdn.7tv.app/emote/id")] },
            "user": { "id": "provider-user", "connections": [
                { "id": "42", "platform": "KICK" },
                { "id": "99", "platform": "TWITCH" },
                { "id": "42", "platform": "TWITCH" }
            ]}
        });
        let state = parse_7tv_channel(response.clone(), "TWITCH", "42").unwrap();
        assert_eq!(state.user_id.as_deref(), Some("provider-user"));
        assert_eq!(state.set_id.as_deref(), Some("nested-set"));
        assert_eq!(state.connection_index, Some(2));
        assert_eq!(state.emotes.len(), 1);

        let mut conflicting = response.clone();
        conflicting["emote_set_id"] = serde_json::json!("different-set");
        assert!(parse_7tv_channel(conflicting, "TWITCH", "42").is_none());

        let mut kick = response;
        kick.as_object_mut().unwrap().remove("emote_set_id");
        kick["platform"] = serde_json::json!("KICK");
        kick["emote_set"]["id"] = serde_json::json!("kick-set");
        let state = parse_7tv_channel(kick, "KICK", "42").unwrap();
        assert_eq!(state.set_id.as_deref(), Some("kick-set"));
        assert_eq!(state.connection_index, Some(0));
    }

    #[test]
    fn missing_channel_assignments_are_empty_but_malformed_snapshots_fail() {
        let state = parse_7tv_channel(serde_json::json!({
            "id": "42", "platform": "TWITCH", "emote_set": null,
            "user": { "id": "provider-user", "connections": [{ "id": "42", "platform": "TWITCH" }] }
        }), "TWITCH", "42").unwrap();
        assert!(state.emotes.is_empty());
        assert_eq!(state.user_id.as_deref(), Some("provider-user"));
        assert_eq!(state.connection_index, Some(0));
        assert!(state.set_id.is_none());
        assert!(parse_7tv_channel(
            serde_json::json!({ "id": "42", "platform": "TWITCH" }),
            "TWITCH",
            "42"
        )
        .unwrap()
        .emotes
        .is_empty());
        for value in [
            serde_json::json!({}),
            serde_json::json!(null),
            serde_json::json!({ "emote_set": {} }),
            serde_json::json!({ "emote_set": "invalid" }),
            serde_json::json!({ "emote_set_id": "assigned", "emote_set": null }),
            serde_json::json!({ "emote_set_id": "../invalid", "emote_set": { "emotes": [] } }),
        ] {
            assert!(parse_7tv_channel(value, "TWITCH", "42").is_none());
        }
    }

    #[test]
    fn empty_sets_can_omit_the_emotes_field_only_with_a_valid_identity() {
        for value in [
            serde_json::json!({ "id": "empty-set" }),
            serde_json::json!({ "id": "empty-set", "emote_count": 0 }),
            serde_json::json!({ "emotes": [] }),
        ] {
            assert!(parse_7tv_emote_set(value).unwrap().is_empty());
        }
        for value in [
            serde_json::json!({}),
            serde_json::json!({ "id": "" }),
            serde_json::json!({ "id": "../invalid" }),
            serde_json::json!({ "id": "x".repeat(129) }),
            serde_json::json!({ "id": "set", "emote_count": 2 }),
            serde_json::json!({ "id": "set", "emotes": null }),
        ] {
            assert!(parse_7tv_emote_set(value).is_none());
        }
        let state = parse_7tv_channel(
            serde_json::json!({ "emote_set": { "id": "empty-set" } }),
            "TWITCH",
            "42",
        )
        .unwrap();
        assert_eq!(state.set_id.as_deref(), Some("empty-set"));
        assert!(state.emotes.is_empty());
    }

    #[test]
    fn shared_emote_parser_accepts_absolute_urls_and_preserves_scale_selection() {
        let relative = parse_7tv_emote(active_emote("//cdn.7tv.app/emote/id/")).unwrap();
        let absolute_value = active_emote("https://cdn.7tv.app/emote/id/");
        let absolute = parse_7tv_emote(absolute_value.clone()).unwrap();
        assert_eq!(absolute, relative);
        assert_eq!(absolute.url_1x, "https://cdn.7tv.app/emote/id/2x.webp");
        assert_eq!(
            absolute.url_2x.as_deref(),
            Some("https://cdn.7tv.app/emote/id/2x.webp")
        );
        assert_eq!(
            parse_7tv_emote_set(serde_json::json!({ "emotes": [absolute_value] })).unwrap(),
            vec![absolute]
        );
        for host in [
            "invalid",
            "data:text/plain,emote",
            "https://user:secret@example.test/emote",
        ] {
            assert!(parse_7tv_emote(active_emote(host)).is_none());
        }
    }

    #[test]
    fn bttv_overlay_codes_apply_only_to_the_global_catalog() {
        let parse = |code: &str, global| {
            BttvEmote {
                id: "same-id".into(),
                code: code.into(),
                image_type: "png".into(),
            }
            .into_info(global)
        };
        for code in [
            "SoSnowy",
            "IceCold",
            "SantaHat",
            "TopHat",
            "ReinDeer",
            "CandyCane",
            "cvMask",
            "cvHazmat",
        ] {
            assert!(parse(code, true).zero_width, "global {code} must overlay");
            assert!(
                !parse(code, false).zero_width,
                "channel/shared {code} must stay ordinary"
            );
        }
        for code in ["SantaHatChannel", "santahat", "Ordinary"] {
            assert!(!parse(code, true).zero_width);
            assert!(!parse(code, false).zero_width);
        }
    }

    #[test]
    fn seven_tv_overlays_use_active_set_flags_and_old_metadata_stays_compatible() {
        for (flags, data_flags, expected) in [
            (0, 256, false),
            (1, 0, true),
            (257, 0, true),
            (256, 256, false),
        ] {
            let mut value = active_emote("//cdn.7tv.app/emote/id");
            value["flags"] = flags.into();
            value["data"]["flags"] = data_flags.into();
            let emote = parse_7tv_emote(value.clone()).unwrap();
            assert_eq!(emote.zero_width, expected);
            let set = parse_7tv_emote_set(serde_json::json!({"emotes": [value]})).unwrap();
            assert_eq!(set[0], emote);
            let decoded: EmoteInfo =
                serde_json::from_str(&serde_json::to_string(&emote).unwrap()).unwrap();
            assert_eq!(decoded, emote);
        }
        let emote = parse_7tv_emote(active_emote("//cdn.7tv.app/emote/id")).unwrap();
        assert!(!emote.zero_width);
        let mut legacy = serde_json::to_value(emote).unwrap();
        legacy.as_object_mut().unwrap().remove("zero_width");
        assert!(
            !serde_json::from_value::<EmoteInfo>(legacy)
                .unwrap()
                .zero_width
        );
    }

    #[tokio::test]
    async fn global_set_fetch_retains_its_identity_and_distinguishes_failure_from_empty() {
        use tokio::io::AsyncWriteExt;
        let provider = SevenTvProvider::with_client(
            reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(2))
                .build()
                .unwrap(),
        );
        for (status, body, succeeded) in [
            (404, "", false),
            (503, "{}", false),
            (200, "{}", false),
            (200, "{\"id\":\"../invalid\",\"emotes\":[]}", false),
            (200, "{\"id\":\"global-set\",\"emotes\":[]}", true),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http_headers(&mut stream).await;
                stream.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            });
            let state = provider
                .fetch_global_state(&format!("http://{address}/global"))
                .await;
            assert_eq!(state.is_some(), succeeded);
            if let Some(state) = state {
                assert_eq!(state.id, "global-set");
                assert!(state.emotes.is_empty());
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn channel_state_fetch_preserves_empty_success_and_failure() {
        use tokio::io::AsyncWriteExt;
        let provider = SevenTvProvider::with_client(
            reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(2))
                .build()
                .unwrap(),
        );
        for (status, body, succeeded) in [
            (404, "", true),
            (503, "{}", false),
            (200, "invalid", false),
            (200, "{}", false),
            (200, "{\"emote_set\":null}", true),
            (200, "{\"emote_set\":{\"id\":\"empty\"}}", true),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http_headers(&mut stream).await;
                let response = format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                stream.write_all(response.as_bytes()).await.unwrap();
            });
            let state = provider
                .fetch_channel_state(&format!("http://{address}/user"), "TWITCH", "42")
                .await;
            assert_eq!(state.is_some(), succeeded);
            if let Some(state) = state {
                assert!(state.emotes.is_empty());
            }
            server.await.unwrap();
        }
        assert!(provider.load_channel_state("OTHER", "42").await.is_none());
        assert!(provider
            .load_channel_state("TWITCH", "../42")
            .await
            .is_none());
    }

    #[test]
    fn personal_sets_are_selected_by_the_personal_flag_not_the_channel_set() {
        let response = serde_json::json!({
            "emote_set": { "id": "channel" },
            "user": { "emote_sets": [
                { "id": "regular", "flags": 0 },
                { "id": "personal", "flags": 4 }
            ]}
        });
        assert_eq!(personal_emote_set_id(&response), Some("personal"));
        assert_eq!(personal_emote_set_id(&response["user"]), Some("personal"));
        assert!(personal_emote_set_id(&serde_json::json!({
            "emote_set": { "id": "channel" },
            "user": { "emote_sets": [{ "id": "regular", "flags": 0 }] }
        }))
        .is_none());
    }

    async fn read_http_headers(stream: &mut tokio::net::TcpStream) {
        use tokio::io::AsyncReadExt;
        let mut request = Vec::new();
        let mut chunk = [0; 512];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let received = stream.read(&mut chunk).await.unwrap();
            assert!(received > 0, "client closed before sending HTTP headers");
            request.extend_from_slice(&chunk[..received]);
            assert!(request.len() <= 8192, "unexpectedly large test request");
        }
    }

    #[tokio::test]
    async fn metadata_reads_reject_oversized_bodies_with_or_without_content_length() {
        use tokio::io::AsyncWriteExt;

        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        for (chunked, limit, accepted) in [(false, 16, false), (true, 16, false), (true, 128, true)]
        {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http_headers(&mut stream).await;
                let body = br#"{"data":"metadata longer than the configured limit"}"#;
                let response = if chunked {
                    format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                        body.len(), std::str::from_utf8(body).unwrap())
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        std::str::from_utf8(body).unwrap()
                    )
                };
                stream.write_all(response.as_bytes()).await.unwrap();
            });
            let response = client
                .get(format!("http://{address}/user"))
                .send()
                .await
                .unwrap();
            assert_eq!(
                read_bounded_json::<serde_json::Value>(response, limit)
                    .await
                    .is_some(),
                accepted
            );
            server.await.unwrap();
        }
    }

    fn parse_7tv_set(value: serde_json::Value) -> Vec<EmoteInfo> {
        let set: EmoteSetResp = serde_json::from_value(value).expect("valid emote set json");
        SevenTvProvider::parse_emote_set(set)
    }

    #[tokio::test]
    async fn channel_json_distinguishes_missing_sets_from_failed_refreshes() {
        use tokio::io::AsyncWriteExt;
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        for (status, body, succeeded, absent) in [
            (404, "", true, true),
            (503, "{}", false, false),
            (200, "invalid", false, false),
            (200, "{\"emotes\":[]}", true, false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http_headers(&mut stream).await;
                let response = format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                stream.write_all(response.as_bytes()).await.unwrap();
            });
            let result =
                fetch_channel_json::<EmoteSetResp>(client.get(format!("http://{address}/set")))
                    .await;
            assert_eq!(result.is_some(), succeeded);
            assert_eq!(matches!(result, Some(None)), absent);
            server.await.unwrap();
        }
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
        assert_eq!(e.url_1x, "https://cdn.7tv.app/emote/abc/2x.webp");
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
    fn seven_tv_normal_assets_keep_animation_and_ignore_unsupported_formats() {
        for reverse in [false, true] {
            let mut files = vec![
                serde_json::json!({ "name": "2x.avif", "width": 64 }),
                serde_json::json!({ "name": "2x.png", "width": 64 }),
                serde_json::json!({ "name": "4x.webp", "width": 128 }),
                serde_json::json!({ "name": "2x.webp", "width": 64 }),
                serde_json::json!({ "name": "1x.webp", "width": 32 }),
            ];
            if reverse {
                files.reverse();
            }
            let mut value = active_emote("//cdn.7tv.app/emote/id");
            value["data"]["host"]["files"] = serde_json::json!(files);
            let emote = parse_7tv_emote(value).unwrap();
            assert_eq!(emote.url_1x, "https://cdn.7tv.app/emote/id/2x.webp");
            assert_eq!(emote.url_2x.as_deref(), Some(emote.url_1x.as_str()));
            assert_eq!(
                emote.url_4x.as_deref(),
                Some("https://cdn.7tv.app/emote/id/4x.webp")
            );
        }
    }

    #[test]
    fn seven_tv_normal_assets_fall_back_when_sizes_or_formats_are_missing() {
        for (names, expected) in [
            (vec!["4x.webp", "1x.webp", "3x.webp"], Some("3x.webp")),
            (vec!["4x.webp", "1x.webp"], Some("4x.webp")),
            (vec!["1x.webp"], Some("1x.webp")),
            (vec!["2x.avif", "1x.gif"], Some("1x.gif")),
            (vec!["2x.png", "1x.webp"], Some("1x.webp")),
            (vec!["2x.PNG"], Some("2x.PNG")),
            (vec!["1x.avif", "2x.avif"], None),
            (Vec::new(), None),
        ] {
            let mut value = active_emote("//cdn.7tv.app/emote/id");
            value["data"]["host"]["files"] = serde_json::json!(names
                .iter()
                .map(|name| serde_json::json!({ "name": name }))
                .collect::<Vec<_>>());
            let emote = parse_7tv_emote(value);
            assert_eq!(
                emote.as_ref().map(|emote| emote.url_1x.as_str()),
                expected
                    .map(|name| format!("https://cdn.7tv.app/emote/id/{name}"))
                    .as_deref()
            );
        }

        let mut value = active_emote("//cdn.7tv.app/emote/id");
        value["data"]["host"]["files"] = serde_json::json!([
            { "name": "small.avif", "width": 16 },
            { "name": "large.webp", "width": 128 },
            { "name": "small.webp", "width": 64 }
        ]);
        assert_eq!(
            parse_7tv_emote(value).unwrap().url_1x,
            "https://cdn.7tv.app/emote/id/small.webp"
        );
    }

    #[test]
    #[ignore = "synthetic animation decode comparison; run with --release --nocapture"]
    fn perf_seven_tv_normal_asset_decode() {
        use image::AnimationDecoder;
        use std::io::Cursor;
        use std::time::Instant;

        fn animation(size: u32) -> Vec<u8> {
            let mut bytes = Vec::new();
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
            for phase in 0..24 {
                let pixels = image::RgbaImage::from_fn(size, size, |x, y| {
                    let logical_x = x * 32 / size;
                    let logical_y = y * 32 / size;
                    image::Rgba([
                        ((logical_x + phase) * 7) as u8,
                        ((logical_y + phase) * 7) as u8,
                        ((logical_x + logical_y) * 3) as u8,
                        255,
                    ])
                });
                encoder
                    .encode_frame(image::Frame::from_parts(
                        pixels,
                        0,
                        0,
                        image::Delay::from_numer_denom_ms(40, 1),
                    ))
                    .unwrap();
            }
            drop(encoder);
            bytes
        }

        for (label, size) in [("normal_2x", 64), ("preview_4x", 128)] {
            let raw = animation(size);
            let mut micros = Vec::new();
            let mut decoded_bytes = 0;
            for _ in 0..15 {
                let started = Instant::now();
                let frames = image::codecs::gif::GifDecoder::new(Cursor::new(&raw))
                    .unwrap()
                    .into_frames()
                    .collect_frames()
                    .unwrap();
                micros.push(started.elapsed().as_micros());
                decoded_bytes = frames
                    .iter()
                    .map(|frame| frame.buffer().as_raw().len())
                    .sum::<usize>();
                assert_eq!(frames.len(), 24);
            }
            micros.sort_unstable();
            assert_eq!(decoded_bytes, size as usize * size as usize * 4 * 24);
            println!(
                "{label}: encoded_bytes={} decoded_bytes={decoded_bytes} median_decode_us={}",
                raw.len(),
                micros[micros.len() / 2]
            );
        }
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
        assert_eq!(emotes[0].url_1x, "https://cdn.7tv.app/emote/def/2x.webp");
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

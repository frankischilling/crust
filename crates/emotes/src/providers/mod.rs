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
        Self { client: reqwest::Client::new() }
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
    fn name(&self) -> &'static str { "bttv" }

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
        let url = format!(
            "https://api.betterttv.net/3/cached/users/twitch/{channel_id}"
        );
        #[derive(Deserialize)]
        struct BttvChannel {
            #[serde(rename = "channelEmotes")]
            channel_emotes: Vec<BttvEmote>,
            #[serde(rename = "sharedEmotes")]
            shared_emotes: Vec<BttvEmote>,
        }
        match self.client.get(&url).send().await {
            Ok(resp) if !resp.status().is_success() => {
                tracing::debug!("BTTV channel returned HTTP {} for {channel_id}", resp.status());
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
        Self { client: reqwest::Client::new() }
    }
}

#[async_trait::async_trait]
impl EmoteProvider for FfzProvider {
    fn name(&self) -> &'static str { "ffz" }

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
                tracing::debug!("FFZ channel returned HTTP {} for {channel_id}", resp.status());
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
        Self { client: reqwest::Client::new() }
    }
}

#[async_trait::async_trait]
impl EmoteProvider for SevenTvProvider {
    fn name(&self) -> &'static str { "7tv" }

    async fn load_global(&self) -> Vec<EmoteInfo> {
        let url = "https://7tv.io/v3/emote-sets/global";
        self.fetch_emote_set(url).await
    }

    async fn load_channel(&self, channel_id: &str) -> Vec<EmoteInfo> {
        let url = format!("https://7tv.io/v3/users/twitch/{channel_id}");
        #[derive(Deserialize)]
        struct UserResp {
            emote_set: Option<EmoteSetResp>,
        }
        match self.client.get(&url).send().await {
            Ok(r) if !r.status().is_success() => {
                tracing::debug!("7TV channel returned HTTP {} for {channel_id}", r.status());
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
                    tracing::warn!("7tv channel parse failed: {e}");
                    vec![]
                }
            },
            Err(e) => {
                tracing::warn!("7tv channel fetch failed: {e}");
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
}

impl SevenTvProvider {
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

    fn parse_emote_set(set: EmoteSetResp) -> Vec<EmoteInfo> {
        set.emotes
            .into_iter()
            .filter_map(|e| {
                let data = e.data?;
                let base = format!("https:{}", data.host.url);
                let file_1x = data.host.files.first().map(|f| format!("{base}/{}", f.name));
                let file_2x = data.host.files.get(1).map(|f| format!("{base}/{}", f.name));
                let file_4x = data.host.files.get(2).map(|f| format!("{base}/{}", f.name));
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

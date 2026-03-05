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
/// IDs are sourced from <https://www.streamdatabase.com/twitch/global-emotes>
/// and verified against the Twitch CDN. Classic numeric IDs and newer
/// `emotesv2_*` IDs are both included.
fn twitch_global_emotes() -> Vec<EmoteInfo> {
    vec![
        // ── Classic numeric IDs (all verified against CDN) ──────────────
        tw("354", "4Head"),
        tw("3792", "ANELE"),
        tw("51838", "ArgieB8"),
        tw("50", "ArsonNoSexy"),
        tw("74", "AsianGlow"),
        tw("22639", "BabyRage"),
        tw("1904", "BigBrother"),
        tw("114738", "BlargNaut"),
        tw("69", "BloodTrail"),
        tw("115233", "BrainSlug"),
        tw("4057", "BrokeBack"),
        tw("27602", "BuddhaBar"),
        tw("166266", "CarlSmile"),
        tw("185914", "ChefFrank"),
        tw("58127", "CoolCat"),
        tw("123171", "CoolStoryBob"),
        tw("49106", "CorgiDerp"),
        tw("116625", "CurseLit"),
        tw("973", "DAESuppy"),
        tw("33", "DansGame"),
        tw("170", "DatSheffy"),
        tw("73", "DBstyle"),
        tw("58135", "DendiFace"),
        tw("959018", "EarthDay"),
        tw("376765", "EntropyWins"),
        tw("360612", "FailFish"),
        tw("65", "FrankerZ"),
        tw("117701", "FreakinStinkin"),
        tw("244", "FUNgineer"),
        tw("98562", "FutureMan"),
        tw("112291", "GivePLZ"),
        tw("112290", "GoatEmotey"),
        tw("3632", "GrammarKing"),
        tw("20225", "HassaanChop"),
        tw("30259", "HeyGuys"),
        tw("357", "HotPokket"),
        tw("160396", "InuyoFace"),
        tw("133468", "ItsBoshyTime"),
        tw("15", "JKanStyle"),
        tw("114836", "Jebaited"),
        tw("25", "Kappa"),
        tw("55338", "KappaPride"),
        tw("70433", "KappaRoss"),
        tw("160395", "Kappu"),
        tw("1902", "Keepo"),
        tw("40", "KevinTurtle"),
        tw("1901", "Kippa"),
        tw("81273", "KomodoHype"),
        tw("160400", "KonCha"),
        tw("41", "Kreygasm"),
        tw("425618", "LUL"),
        tw("1290325", "MaxLOL"),
        tw("110785", "MercyWing1"),
        tw("110786", "MercyWing2"),
        tw("81636", "MikeHogu"),
        tw("68856", "MingLee"),
        tw("156787", "MorphinTime"),
        tw("28", "MrDestructoid"),
        tw("133537", "MVGame"),
        tw("138325", "NinjaGrumpy"),
        tw("90075", "NomNom"),
        tw("34875", "NotATK"),
        tw("16", "OptimizePrime"),
        tw("356", "OpieOP"),
        tw("81248", "OSFrog"),
        tw("965738", "PartyHat"),
        tw("112289", "pastaThat"),
        tw("3412", "PeoplesChamp"),
        tw("27509", "PermaSmug"),
        tw("4240", "PipeHype"),
        tw("36", "PJSalt"),
        tw("92", "PMSTwin"),
        tw("305954156", "PogChamp"),
        tw("358", "Poooound"),
        tw("724216", "PopCorn"),
        tw("38586", "PraiseIt"),
        tw("28328", "PRChase"),
        tw("115075", "PrimeMe"),
        tw("17", "PunchTrees"),
        tw("160401", "PunOko"),
        tw("114870", "RaccAttack"),
        tw("1900", "RalpherZ"),
        tw("22998", "RedCoat"),
        tw("245", "ResidentSleeper"),
        tw("160402", "SabaPing"),
        tw("64138", "SeemsGood"),
        tw("81249", "SeriousSloth"),
        tw("87", "ShazBotstix"),
        tw("300116349", "SingsMic"),
        tw("300116350", "SingsNote"),
        tw("52", "SMOrc"),
        tw("89945", "SmoocherZ"),
        tw("1906", "SoBayed"),
        tw("2113050", "SoonerLater"),
        tw("191762", "Squid1"),
        tw("191763", "Squid2"),
        tw("191764", "Squid3"),
        tw("191767", "Squid4"),
        tw("46", "SSSsss"),
        tw("34", "SwiftRage"),
        tw("112292", "TakeNRG"),
        tw("160403", "TearGlove"),
        tw("160404", "TehePelo"),
        tw("1899", "TF2John"),
        tw("18145", "TheRinger"),
        tw("7427", "TheThing"),
        tw("1898", "ThunBeast"),
        tw("120232", "TriHard"),
        tw("38436", "TTours"),
        tw("166263", "TwitchLit"),
        tw("300116344", "TwitchSings"),
        tw("196892", "TwitchUnity"),
        tw("81274", "VoHiYo"),
        tw("106293", "VoteYea"),
        tw("106294", "VoteNay"),
        tw("28087", "WutFace"),
        tw("5", "YouDontSay"),
        tw("4337", "YouWHY"),
        tw("62835", "bleedPurple"),
        tw("84608", "cmonBruh"),
        tw("112288", "copyThis"),
        tw("62834", "duDudu"),
        tw("35063", "mcaT"),
        // ── emotesv2_* IDs (verified via streamdatabase.com) ────────────
        tw("emotesv2_ed53f0877c984ddcadfa500347b1fd0c", "AndalusianCrush"),
        tw("emotesv2_50b3304bc0884c6792f13615db072a5c", "CaitThinking"),
        tw("emotesv2_d351c5d5e9084402b30bc39eaa3d92ae", "Cinheimer"),
        tw("emotesv2_dcd06b30a5c24f6eb871e8f5edbd44f7", "DinoDance"),
        tw("emotesv2_cb0cec2c9975497fa093be1c3276dd92", "EleGiggle"),
        tw("emotesv2_fc2f39b6a62c4d7e832993eab17547d2", "FeverFighter"),
        tw("emotesv2_c1f4899e65cf4f53b2fd98e15733973a", "GoldPLZ"),
        tw("emotesv2_973cf33af6e14a92b7c5b970a9d55afd", "GRASSLORD"),
        tw("emotesv2_b0c6ccb3b12b4f99a9cc83af365a09f1", "ImTyping"),
        tw("emotesv2_031bf329c21040a897d55ef471da3dd3", "Jebasted"),
        tw("emotesv2_ecb0bfd49b3c4325864b948d46c8152b", "LaundryBasket"),
        tw("emotesv2_a2dfbbbbf66f4a75b0f53db841523e6c", "ModLove"),
        tw("emotesv2_c0c9c932c82244ff920ad2134be90afb", "MyAvatar"),
        tw("emotesv2_53f6a6af8b0e453d874bbefee49b3e73", "NewRecord"),
        tw("emotesv2_3db6b02aabee45c69aefe85337569035", "NowField"),
        tw("emotesv2_587405136a8147148c77df74baaa1bf4", "PewPewPew"),
        tw("emotesv2_f202746ed88f4e7c872b50b1f7fd78cc", "PizzaTime"),
        tw("emotesv2_5d523adb8bbb4786821cd7091e47da21", "PopNemo"),
        tw("emotesv2_819621bcb8f44566a1bd8ea63d06c58f", "Shush"),
        tw("emotesv2_6581608ddd89425eb4374a34b6b4337e", "SimsPlumbob"),
        tw("emotesv2_ba5ae4be5c724ca59d649fa713ff0730", "SipTime"),
        tw("emotesv2_fcbeed664f7c47d6ba3b57691275ef51", "SUBprise"),
        tw("emotesv2_bfb533e2253044f3a77d0032b2354c0b", "SUBtember"),
        tw("emotesv2_13b6dd7f3a3146ef8dc10f66d8b42a96", "TwitchConHYPE"),
        tw("emotesv2_5fa0f9da251941988f31b6d7632c021c", "TWITH"),
        tw("emotesv2_a7ab2c184e334d4a9784e6e5d51947f7", "WeDidThat"),
        tw("emotesv2_3ffb1454600e4a33bd99a8b6894e6737", "WoWMidnight"),
        tw("emotesv2_de2cc5fc92c645a29d0a4f40a9e7cde5", "Yagoo"),
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
                let base = format!("https:{}", data.host.url);
                let file_1x = data
                    .host
                    .files
                    .first()
                    .map(|f| format!("{base}/{}", f.name));
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

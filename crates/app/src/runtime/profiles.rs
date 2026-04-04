use crust_core::{
    events::AppEvent,
    model::{ChannelId, UserProfile},
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::runtime::assets::fetch_and_decode_raw;

fn non_empty_opt(input: Option<String>) -> Option<String> {
    input.and_then(|v| {
        let trimmed = v.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    })
}

/// Fetch a fresh Twitch live viewer count via public GQL.
///
/// IVR can lag for hot channels, so this gives us a closer-to-live count
/// without requiring user OAuth.
async fn fetch_twitch_live_viewer_count_gql(login: &str) -> Option<u64> {
    #[derive(serde::Deserialize)]
    struct GqlResponse {
        data: Option<GqlData>,
    }

    #[derive(serde::Deserialize)]
    struct GqlData {
        user: Option<GqlUser>,
    }

    #[derive(serde::Deserialize)]
    struct GqlUser {
        stream: Option<GqlStream>,
    }

    #[derive(serde::Deserialize)]
    struct GqlStream {
        #[serde(rename = "viewersCount")]
        viewers_count: Option<u64>,
    }

    let login = login.trim().to_ascii_lowercase();
    if login.is_empty() {
        return None;
    }

    let payload = serde_json::json!({
        "query": "query($login:String!){user(login:$login){stream{viewersCount}}}",
        "variables": { "login": login },
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let resp = client
        .post("https://gql.twitch.tv/gql")
        .header("Client-ID", "kimne78kx3ncx6brgo4mv6wki5h1ko")
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
        .header(reqwest::header::PRAGMA, "no-cache")
        .json(&payload)
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let parsed: GqlResponse = resp.json().await.ok()?;
    parsed.data?.user?.stream?.viewers_count
}

/// Fetch a user profile appropriate for the channel platform.
pub(crate) async fn fetch_user_profile_for_channel(
    login: &str,
    channel: &ChannelId,
    oauth_token: Option<&str>,
    client_id: Option<&str>,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    if channel.is_kick() {
        fetch_kick_user_profile(login, evt_tx).await;
    } else if channel.is_irc() {
        let _ = evt_tx
            .send(AppEvent::UserProfileUnavailable {
                login: login.to_owned(),
            })
            .await;
    } else {
        fetch_twitch_user_profile(login, oauth_token, client_id, evt_tx).await;
    }
}

#[derive(Debug, Clone)]
struct TwitchStreamSnapshot {
    is_live: bool,
    title: Option<String>,
    game: Option<String>,
    viewers: Option<u64>,
}

/// Fetch a Twitch stream snapshot via Helix streams endpoint.
///
/// Returns `Some` for both live and offline channels when the request itself
/// succeeds. Returns `None` only when Helix could not be queried.
async fn fetch_twitch_stream_snapshot_helix(
    login: &str,
    oauth_token: Option<&str>,
    client_id: Option<&str>,
) -> Option<TwitchStreamSnapshot> {
    #[derive(serde::Deserialize)]
    struct HelixStreamItem {
        title: String,
        #[serde(rename = "game_name")]
        game_name: String,
        #[serde(rename = "viewer_count")]
        viewer_count: u64,
    }

    #[derive(serde::Deserialize)]
    struct HelixStreamsResponse {
        data: Vec<HelixStreamItem>,
    }

    let token = oauth_token.map(str::trim).filter(|s| !s.is_empty());
    let Some(token) = token else {
        warn!("stream-status helix unavailable: missing oauth token");
        return None;
    };

    let client_id = client_id.map(str::trim).filter(|s| !s.is_empty());
    let Some(client_id) = client_id else {
        warn!("stream-status helix unavailable: missing client id");
        return None;
    };

    let login = login.trim().to_ascii_lowercase();
    if login.is_empty() {
        return None;
    }

    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    if bare.trim().is_empty() {
        return None;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let resp = client
        .get("https://api.twitch.tv/helix/streams")
        .query(&[("user_login", login.as_str())])
        .header("Authorization", format!("Bearer {bare}"))
        .header("Client-Id", client_id)
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
        .header(reqwest::header::PRAGMA, "no-cache")
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            warn!("stream-status helix request failed for {login}: {e}");
            return None;
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        warn!("stream-status helix HTTP {status} for {login}: {body}");
        return None;
    }

    let parsed: HelixStreamsResponse = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            warn!("stream-status helix decode failed for {login}: {e}");
            return None;
        }
    };
    let snapshot = match parsed.data.into_iter().next() {
        Some(item) => TwitchStreamSnapshot {
            is_live: true,
            title: non_empty_opt(Some(item.title)),
            game: non_empty_opt(Some(item.game_name)),
            viewers: Some(item.viewer_count),
        },
        None => TwitchStreamSnapshot {
            is_live: false,
            title: None,
            game: None,
            viewers: None,
        },
    };

    Some(snapshot)
}

/// Fetch a Twitch stream snapshot via IVR user endpoint.
///
/// Returns `Some` for both live and offline channels when IVR returns a user.
/// Returns `None` for network/protocol failures.
async fn fetch_twitch_stream_snapshot_ivr(login: &str) -> Option<TwitchStreamSnapshot> {
    #[derive(serde::Deserialize)]
    struct IvrStreamGame {
        #[serde(rename = "displayName", default)]
        display_name: String,
    }

    #[derive(serde::Deserialize)]
    struct IvrStream {
        #[serde(default)]
        title: String,
        game: Option<IvrStreamGame>,
        #[serde(rename = "viewersCount", default)]
        viewers_count: u64,
    }

    #[derive(serde::Deserialize)]
    struct IvrUser {
        stream: Option<IvrStream>,
    }

    let login = login.trim().to_ascii_lowercase();
    if login.is_empty() {
        return None;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let resp = client
        .get(format!("https://api.ivr.fi/v2/twitch/user?login={login}"))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let users: Vec<IvrUser> = resp.json().await.ok()?;
    let user = users.into_iter().next()?;

    let snapshot = match user.stream {
        Some(stream) => TwitchStreamSnapshot {
            is_live: true,
            title: non_empty_opt(Some(stream.title)),
            game: non_empty_opt(stream.game.map(|g| g.display_name)),
            viewers: Some(stream.viewers_count),
        },
        None => TwitchStreamSnapshot {
            is_live: false,
            title: None,
            game: None,
            viewers: None,
        },
    };

    Some(snapshot)
}

/// Fetch Twitch stream status for periodic channel refresh.
///
/// Uses Helix streams as the primary source and falls back to IVR when Helix
/// is unavailable. Emits `StreamStatusUpdated` on success.
pub(crate) async fn fetch_twitch_stream_status(
    login: &str,
    oauth_token: Option<&str>,
    client_id: Option<&str>,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let login = login.trim().to_ascii_lowercase();
    if login.is_empty() {
        let _ = evt_tx
            .send(AppEvent::UserProfileUnavailable {
                login: login.to_owned(),
            })
            .await;
        return;
    }

    debug!("stream-status refresh requested for {login}");

    let mut source = "helix";
    let mut snapshot = fetch_twitch_stream_snapshot_helix(&login, oauth_token, client_id).await;
    if snapshot.is_none() {
        source = "ivr";
        snapshot = fetch_twitch_stream_snapshot_ivr(&login).await;
    }

    if let Some(mut snapshot) = snapshot {
        if snapshot.is_live {
            if let Some(gql_viewers) = fetch_twitch_live_viewer_count_gql(&login).await {
                if snapshot.viewers != Some(gql_viewers) {
                    debug!(
                        "stream-status viewer override from gql for {login}: {:?} -> {}",
                        snapshot.viewers, gql_viewers
                    );
                }
                snapshot.viewers = Some(gql_viewers);
                source = "gql";
            }
        }

        debug!(
            "stream-status refresh result for {login}: source={source}, live={}, viewers={:?}",
            snapshot.is_live, snapshot.viewers
        );
        let _ = evt_tx
            .send(AppEvent::StreamStatusUpdated {
                login,
                is_live: snapshot.is_live,
                title: snapshot.title,
                game: snapshot.game,
                viewers: snapshot.viewers,
            })
            .await;
    } else {
        warn!("stream-status refresh failed for {login}");
        let _ = evt_tx
            .send(AppEvent::UserProfileUnavailable {
                login: login.to_owned(),
            })
            .await;
    }
}

/// Fetch a Twitch user profile from the IVR API (no auth required) and send
/// `AppEvent::UserProfileLoaded`. Also pre-fetches avatar bytes so the popup
/// can show the real avatar immediately.
pub(crate) async fn fetch_twitch_user_profile(
    login: &str,
    oauth_token: Option<&str>,
    client_id: Option<&str>,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    #[derive(serde::Deserialize)]
    struct IvrRoles {
        #[serde(rename = "isPartner", default)]
        is_partner: bool,
        #[serde(rename = "isAffiliate", default)]
        is_affiliate: bool,
        #[serde(rename = "isBanned", default)]
        is_banned: bool,
    }

    #[derive(serde::Deserialize)]
    struct IvrStreamGame {
        #[serde(rename = "displayName", default)]
        display_name: String,
    }

    #[derive(serde::Deserialize)]
    struct IvrStream {
        #[serde(default)]
        title: String,
        /// IVR v2 returns game as an object {displayName: "..."}.
        game: Option<IvrStreamGame>,
        /// IVR uses "viewersCount" in v2.
        #[serde(rename = "viewersCount", default)]
        viewers_count: u64,
        #[serde(rename = "startedAt")]
        started_at: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct IvrBroadcast {
        #[serde(rename = "startedAt")]
        started_at: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct IvrBanStatus {
        reason: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct IvrUser {
        #[serde(default)]
        id: String,
        #[serde(default)]
        login: String,
        #[serde(rename = "displayName", default)]
        display_name: String,
        #[serde(default)]
        description: String,
        #[serde(rename = "createdAt")]
        created_at: Option<String>,
        logo: Option<String>,
        #[serde(default)]
        followers: Option<u64>,
        #[serde(default)]
        roles: Option<IvrRoles>,
        /// User's chosen chat colour, e.g. `"#FF6905"`.
        #[serde(rename = "chatColor")]
        chat_color: Option<String>,
        /// Optional pronouns label from profile providers.
        #[serde(default, alias = "pronouns", alias = "pronoun")]
        pronouns: Option<String>,
        /// Optional follow timestamp for this channel context.
        #[serde(
            default,
            alias = "followedAt",
            alias = "followed_at",
            alias = "followingSince"
        )]
        followed_at: Option<String>,
        /// Non-null while the channel is live.
        stream: Option<IvrStream>,
        /// Info about the most recent broadcast.
        #[serde(rename = "lastBroadcast")]
        last_broadcast: Option<IvrBroadcast>,
        /// Non-null if the account is banned/suspended.
        #[serde(rename = "banStatus")]
        ban_status: Option<IvrBanStatus>,
    }

    let requested_login = login.trim().to_ascii_lowercase();
    if requested_login.is_empty() {
        let _ = evt_tx
            .send(AppEvent::UserProfileUnavailable {
                login: login.to_owned(),
            })
            .await;
        return;
    }

    let url = format!("https://api.ivr.fi/v2/twitch/user?login={requested_login}");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let resp = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            warn!(
                "IVR user fetch returned HTTP {} for {}",
                r.status(),
                requested_login
            );
            let _ = evt_tx
                .send(AppEvent::UserProfileUnavailable {
                    login: requested_login.clone(),
                })
                .await;
            return;
        }
        Err(e) => {
            warn!("IVR user fetch failed for {}: {e}", requested_login);
            let _ = evt_tx
                .send(AppEvent::UserProfileUnavailable {
                    login: requested_login.clone(),
                })
                .await;
            return;
        }
    };

    let users: Vec<IvrUser> = match resp.json().await {
        Ok(u) => u,
        Err(e) => {
            warn!(
                "IVR user response parse failed for {}: {e}",
                requested_login
            );
            let _ = evt_tx
                .send(AppEvent::UserProfileUnavailable {
                    login: requested_login.clone(),
                })
                .await;
            return;
        }
    };

    let Some(user) = users.into_iter().next() else {
        warn!("IVR returned no user for {}", requested_login);
        let _ = evt_tx
            .send(AppEvent::UserProfileUnavailable {
                login: requested_login.clone(),
            })
            .await;
        return;
    };

    let avatar_url = user.logo.clone();

    let mut is_live = user.stream.is_some();
    let mut stream_title = user
        .stream
        .as_ref()
        .map(|s| s.title.clone())
        .filter(|s| !s.is_empty());
    let mut stream_game = user
        .stream
        .as_ref()
        .and_then(|s| s.game.as_ref())
        .map(|g| g.display_name.clone())
        .filter(|s| !s.is_empty());
    let mut stream_viewers = user.stream.as_ref().map(|s| s.viewers_count);
    let stream_started = user.stream.as_ref().and_then(|s| s.started_at.clone());
    let last_broadcast_at =
        stream_started.or_else(|| user.last_broadcast.and_then(|b| b.started_at));
    let is_banned = user.roles.as_ref().map_or(false, |r| r.is_banned) || user.ban_status.is_some();
    let ban_reason = user.ban_status.and_then(|b| b.reason);

    let snapshot_login = if user.login.trim().is_empty() {
        requested_login.as_str()
    } else {
        user.login.as_str()
    };

    if let Some(snapshot) =
        fetch_twitch_stream_snapshot_helix(snapshot_login, oauth_token, client_id).await
    {
        is_live = snapshot.is_live;
        stream_title = snapshot.title.or(stream_title);
        stream_game = snapshot.game.or(stream_game);
        stream_viewers = snapshot.viewers.or(stream_viewers);
    }

    if is_live {
        let gql_login = if user.login.trim().is_empty() {
            requested_login.as_str()
        } else {
            user.login.as_str()
        };
        if let Some(fresh_viewers) = fetch_twitch_live_viewer_count_gql(gql_login).await {
            if stream_viewers != Some(fresh_viewers) {
                debug!(
                    "profile viewer override from gql for {}: {:?} -> {}",
                    gql_login, stream_viewers, fresh_viewers
                );
            }
            stream_viewers = Some(fresh_viewers);
        }
    }

    let profile_login = user.login.trim().to_ascii_lowercase();
    let profile_login = if profile_login.is_empty() {
        requested_login.clone()
    } else {
        profile_login
    };

    let profile = UserProfile {
        id: user.id,
        login: profile_login,
        display_name: user.display_name,
        description: user.description,
        created_at: user.created_at,
        avatar_url: avatar_url.clone(),
        followers: user.followers,
        is_partner: user.roles.as_ref().map_or(false, |r| r.is_partner),
        is_affiliate: user.roles.as_ref().map_or(false, |r| r.is_affiliate),
        pronouns: non_empty_opt(user.pronouns),
        followed_at: non_empty_opt(user.followed_at),
        chat_color: user.chat_color,
        is_live,
        stream_title,
        stream_game,
        stream_viewers,
        last_broadcast_at,
        is_banned,
        ban_reason,
    };

    let _ = evt_tx.send(AppEvent::UserProfileLoaded { profile }).await;

    // Pre-fetch avatar bytes so egui can display them right away.
    if let Some(ref logo) = avatar_url {
        if let Ok((w, h, raw)) = fetch_and_decode_raw(logo).await {
            let _ = evt_tx
                .send(AppEvent::EmoteImageReady {
                    uri: logo.clone(),
                    width: w,
                    height: h,
                    raw_bytes: raw,
                })
                .await;
        }
    }
}

/// Fetch external chat logs from the IVR logs API (logs.ivr.fi).
/// Fetches the current month's logs in reverse chronological order.
pub(crate) async fn fetch_ivr_logs(channel: &str, username: &str, evt_tx: mpsc::Sender<AppEvent>) {
    use crust_core::events::IvrLogEntry;

    #[derive(serde::Deserialize)]
    struct IvrLogMessage {
        #[serde(default)]
        text: String,
        #[serde(default)]
        timestamp: String,
        #[serde(rename = "displayName", default)]
        display_name: String,
        /// 1 = chat message, 2 = timeout/ban
        #[serde(rename = "type", default)]
        msg_type: u8,
    }

    #[derive(serde::Deserialize)]
    struct IvrLogResponse {
        #[serde(default)]
        messages: Vec<IvrLogMessage>,
    }

    // Fetch current month's logs
    let now = chrono::Utc::now();
    let year = now.format("%Y");
    let month = now.format("%-m");

    let url = format!(
        "https://logs.ivr.fi/channel/{channel}/user/{username}/{year}/{month}?json=true&reverse=true"
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let resp = match client
        .get(&url)
        .header("User-Agent", "crust-chat/1.0")
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            let msg = if status.as_u16() == 404 {
                "No logs found for this user/channel combination.".to_owned()
            } else {
                format!("IVR logs returned HTTP {status}: {body}")
            };
            warn!("{msg}");
            let _ = evt_tx
                .send(AppEvent::IvrLogsFailed {
                    username: username.to_owned(),
                    error: msg,
                })
                .await;
            return;
        }
        Err(e) => {
            warn!("IVR logs fetch failed for {username} in {channel}: {e}");
            let _ = evt_tx
                .send(AppEvent::IvrLogsFailed {
                    username: username.to_owned(),
                    error: format!("Network error: {e}"),
                })
                .await;
            return;
        }
    };

    // Read the full response as text first, then parse JSON.
    // This avoids potential hangs from reqwest's streaming JSON parser
    // and gives us better debug info if something goes wrong.
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            warn!("IVR logs: failed to read response body for {username}: {e}");
            let _ = evt_tx
                .send(AppEvent::IvrLogsFailed {
                    username: username.to_owned(),
                    error: format!("Failed to read response: {e}"),
                })
                .await;
            return;
        }
    };

    let parsed: IvrLogResponse = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                "IVR logs parse failed for {username}: {e} (body len={})",
                body.len()
            );
            let _ = evt_tx
                .send(AppEvent::IvrLogsFailed {
                    username: username.to_owned(),
                    error: format!("Failed to parse response: {e}"),
                })
                .await;
            return;
        }
    };

    let entries: Vec<IvrLogEntry> = parsed
        .messages
        .into_iter()
        .map(|m| IvrLogEntry {
            text: m.text,
            timestamp: m.timestamp,
            display_name: m.display_name,
            msg_type: m.msg_type,
        })
        .collect();

    info!(
        "IVR logs: loaded {} entries for {username} in {channel}",
        entries.len()
    );
    let _ = evt_tx
        .send(AppEvent::IvrLogsLoaded {
            username: username.to_owned(),
            messages: entries,
        })
        .await;
}

/// Fetch a Kick user profile via Kick's public channel API.
pub(crate) async fn fetch_kick_user_profile(login: &str, evt_tx: mpsc::Sender<AppEvent>) {
    #[derive(serde::Deserialize)]
    struct KickCategory {
        #[serde(
            default,
            alias = "display_name",
            alias = "displayName",
            alias = "slug",
            alias = "name"
        )]
        name: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct KickLivestream {
        #[serde(default, alias = "title", alias = "sessionTitle")]
        session_title: Option<String>,
        #[serde(default, alias = "isLive")]
        is_live: Option<bool>,
        #[serde(default, alias = "viewer_count", alias = "viewersCount")]
        viewers_count: Option<u64>,
        #[serde(default, alias = "startedAt")]
        started_at: Option<String>,
        #[serde(default)]
        category: Option<KickCategory>,
    }

    #[derive(serde::Deserialize)]
    struct KickUser {
        #[serde(default)]
        id: Option<u64>,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        slug: Option<String>,
        #[serde(default, alias = "bio", alias = "description")]
        description: Option<String>,
        #[serde(
            default,
            alias = "profilePicture",
            alias = "profile_pic",
            alias = "profilePic",
            alias = "avatar",
            alias = "avatar_url"
        )]
        avatar_url: Option<String>,
        #[serde(default, alias = "createdAt")]
        created_at: Option<String>,
        #[serde(
            default,
            alias = "followersCount",
            alias = "follower_count",
            alias = "followers_count"
        )]
        followers_count: Option<u64>,
        #[serde(default, alias = "isVerified", alias = "verified")]
        is_verified: Option<bool>,
    }

    #[derive(serde::Deserialize)]
    struct KickChannel {
        #[serde(default)]
        id: Option<u64>,
        #[serde(default)]
        slug: Option<String>,
        #[serde(default)]
        user: Option<KickUser>,
        #[serde(default)]
        livestream: Option<KickLivestream>,
        #[serde(default, alias = "description", alias = "bio")]
        description: Option<String>,
        #[serde(
            default,
            alias = "followersCount",
            alias = "follower_count",
            alias = "followers_count"
        )]
        followers_count: Option<u64>,
    }

    fn minimal_kick_profile(login: &str) -> UserProfile {
        UserProfile {
            id: String::new(),
            login: login.to_owned(),
            display_name: login.to_owned(),
            description: String::new(),
            created_at: None,
            avatar_url: None,
            followers: None,
            is_partner: false,
            is_affiliate: false,
            pronouns: None,
            followed_at: None,
            chat_color: None,
            is_live: false,
            stream_title: None,
            stream_game: None,
            stream_viewers: None,
            last_broadcast_at: None,
            is_banned: false,
            ban_reason: None,
        }
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

    let slug = login
        .trim()
        .trim_start_matches('#')
        .trim_start_matches("kick:")
        .to_lowercase();
    let url = format!("https://kick.com/api/v2/channels/{slug}");
    let client = reqwest::Client::new();
    let resp = match client
        .get(&url)
        .header(
            reqwest::header::USER_AGENT,
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        )
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            warn!("Kick user fetch returned HTTP {} for {slug}", r.status());
            let profile = minimal_kick_profile(&slug);
            let _ = evt_tx.send(AppEvent::UserProfileLoaded { profile }).await;
            return;
        }
        Err(e) => {
            warn!("Kick user fetch failed for {slug}: {e}");
            let profile = minimal_kick_profile(&slug);
            let _ = evt_tx.send(AppEvent::UserProfileLoaded { profile }).await;
            return;
        }
    };

    let channel: KickChannel = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            warn!("Kick user response parse failed for {slug}: {e}");
            let profile = minimal_kick_profile(&slug);
            let _ = evt_tx.send(AppEvent::UserProfileLoaded { profile }).await;
            return;
        }
    };

    let user = channel.user;
    let resolved_login = user
        .as_ref()
        .and_then(|u| u.slug.clone().or_else(|| u.username.clone()))
        .or_else(|| channel.slug.clone())
        .unwrap_or_else(|| slug.clone());
    let display_name = user
        .as_ref()
        .and_then(|u| u.username.clone())
        .unwrap_or_else(|| resolved_login.clone());
    let avatar_url = user
        .as_ref()
        .and_then(|u| u.avatar_url.as_deref())
        .map(normalize_kick_url);
    let followers = user
        .as_ref()
        .and_then(|u| u.followers_count)
        .or(channel.followers_count);
    let description = user
        .as_ref()
        .and_then(|u| u.description.clone())
        .or(channel.description)
        .unwrap_or_default();
    let created_at = user.as_ref().and_then(|u| u.created_at.clone());

    let is_live = channel
        .livestream
        .as_ref()
        .map(|s| s.is_live.unwrap_or(true))
        .unwrap_or(false);
    let stream_title = channel
        .livestream
        .as_ref()
        .and_then(|s| s.session_title.clone())
        .filter(|s| !s.is_empty());
    let stream_game = channel
        .livestream
        .as_ref()
        .and_then(|s| s.category.as_ref())
        .and_then(|c| c.name.clone())
        .filter(|s| !s.is_empty());
    let stream_viewers = channel.livestream.as_ref().and_then(|s| s.viewers_count);
    let last_broadcast_at = channel
        .livestream
        .as_ref()
        .and_then(|s| s.started_at.clone());

    let profile = UserProfile {
        id: user
            .as_ref()
            .and_then(|u| u.id)
            .or(channel.id)
            .map(|v| v.to_string())
            .unwrap_or_default(),
        login: resolved_login,
        display_name,
        description,
        created_at,
        avatar_url: avatar_url.clone(),
        followers,
        is_partner: user.as_ref().and_then(|u| u.is_verified).unwrap_or(false),
        is_affiliate: false,
        pronouns: None,
        followed_at: None,
        chat_color: None,
        is_live,
        stream_title,
        stream_game,
        stream_viewers,
        last_broadcast_at,
        is_banned: false,
        ban_reason: None,
    };

    if let Some(ref logo) = avatar_url {
        if let Ok((w, h, raw)) = fetch_and_decode_raw(logo).await {
            let _ = evt_tx
                .send(AppEvent::EmoteImageReady {
                    uri: logo.clone(),
                    width: w,
                    height: h,
                    raw_bytes: raw,
                })
                .await;
        }
    }

    let _ = evt_tx.send(AppEvent::UserProfileLoaded { profile }).await;
}

/// Fetch the logged-in user's avatar URL and image bytes for the top-bar pill.
pub(crate) async fn fetch_self_avatar(login: &str, evt_tx: mpsc::Sender<AppEvent>) {
    if login.is_empty() {
        return;
    }

    #[derive(serde::Deserialize)]
    struct IvrUserMin {
        logo: Option<String>,
    }

    let url = format!("https://api.ivr.fi/v2/twitch/user?login={login}");
    let client = reqwest::Client::new();
    let resp = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return,
    };
    let users: Vec<IvrUserMin> = match resp.json().await {
        Ok(u) => u,
        Err(_) => return,
    };
    let Some(user) = users.into_iter().next() else {
        return;
    };
    let Some(avatar_url) = user.logo else { return };

    // Pre-fetch image bytes
    if let Ok((w, h, raw)) = fetch_and_decode_raw(&avatar_url).await {
        let _ = evt_tx
            .send(AppEvent::EmoteImageReady {
                uri: avatar_url.clone(),
                width: w,
                height: h,
                raw_bytes: raw,
            })
            .await;
    }

    let _ = evt_tx.send(AppEvent::SelfAvatarLoaded { avatar_url }).await;
}

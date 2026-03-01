use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use chrono::Utc;
use crust_core::events::{AppCommand, AppEvent, ConnectionState};
use crust_core::model::{
    ChatMessage, MessageFlags, MessageId, Sender, UserId,
};
use crust_emotes::{
    cache::EmoteCache,
    providers::{BttvProvider, EmoteInfo, FfzProvider, SevenTvProvider},
    EmoteProvider,
};
use crust_storage::SettingsStore;
use crust_twitch::session::client::{SessionCommand, TwitchEvent, TwitchSession};
use crust_ui::CrustApp;

const CMD_CHANNEL_SIZE: usize = 128;
const EVT_CHANNEL_SIZE: usize = 4096;
const TWITCH_EVT_SIZE: usize = 4096;

/// Shared emote index: code → EmoteInfo.
type EmoteIndex = Arc<RwLock<std::collections::HashMap<String, EmoteInfo>>>;

/// Shared badge map: (set_name, version) → image URL.
type BadgeMap = Arc<RwLock<std::collections::HashMap<(String, String), String>>>;



fn main() -> Result<()> {
    // ── Wayland compatibility ────────────────────────────────────────────
    // On pure-Wayland sessions the XDG Settings Portal may not be running.
    // sctk-adwaita (client-side decorations in winit) queries it for the
    // color-scheme and may time out.  The "Io error: Broken pipe" that
    // follows is a known issue and does not affect functionality — we
    // handle it as a benign winit exit below.

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("crust=debug,warn")),
        )
        .init();

    info!("Crust starting up");

    // Channels: UI ↔ runtime
    let (cmd_tx, cmd_rx) = mpsc::channel::<AppCommand>(CMD_CHANNEL_SIZE);
    let (evt_tx, evt_rx) = mpsc::channel::<AppEvent>(EVT_CHANNEL_SIZE);

    // Twitch session channels
    let (tw_evt_tx, tw_evt_rx) = mpsc::channel::<TwitchEvent>(TWITCH_EVT_SIZE);
    let (sess_cmd_tx, sess_cmd_rx) = mpsc::channel::<SessionCommand>(64);

    // Emote index shared between loaders and reducer
    let emote_index: EmoteIndex = Arc::new(RwLock::new(std::collections::HashMap::new()));

    // Emote cache for disk/network
    let emote_cache = EmoteCache::new().ok();

    // Badge map: (set, version) → URL
    let badge_map: BadgeMap = Arc::new(RwLock::new(std::collections::HashMap::new()));

    // Settings / token storage
    let settings_store = SettingsStore::new().ok();
    let saved_token = settings_store.as_ref().and_then(|s| s.load_token());

    // Build tokio runtime on background threads (eframe needs main thread)
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;

    // Spawn anonymous Twitch session
    rt.spawn({
        let session = TwitchSession::new(tw_evt_tx, sess_cmd_rx);
        session.run()
    });

    // Load global emotes in background
    rt.spawn({
        let idx = emote_index.clone();
        let cache = emote_cache.clone();
        async move {
            load_global_emotes(&idx, &cache).await;
        }
    });

    // Load global badges in background
    rt.spawn({
        let bm = badge_map.clone();
        async move {
            load_global_badges(&bm).await;
        }
    });

    // Spawn the reducer (bridges twitch events → tokenized AppEvents for UI)
    rt.spawn({
        let idx = emote_index.clone();
        let cache = emote_cache.clone();
        let bm = badge_map.clone();
        reducer_loop(cmd_rx, tw_evt_rx, evt_tx, sess_cmd_tx, idx, cache, bm, settings_store, saved_token)
    });

    // ── eframe / egui ────────────────────────────────────────────────────
    let native_opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Crust – Twitch Chat")
            .with_inner_size([1100.0, 700.0])
            .with_min_inner_size([600.0, 400.0])
            .with_app_id("crust"),
        ..Default::default()
    };

    let result = eframe::run_native(
        "crust",
        native_opts,
        Box::new(move |cc| Ok(Box::new(CrustApp::new(cc, cmd_tx, evt_rx)))),
    );

    // winit on Wayland may exit with code 1 if the sctk-adwaita CSD
    // portal query fails.  This is harmless – the user closed the window
    // normally and the portal problem is cosmetic.  Treat it as success.
    match result {
        Ok(()) => {}
        Err(e) => {
            let msg = format!("{e}");
            if msg.contains("Exit Failure: 1") {
                // Swallow the false-positive Wayland CSD error.
                tracing::debug!("Ignoring benign winit exit: {msg}");
            } else {
                return Err(anyhow::anyhow!("eframe error: {e}"));
            }
        }
    }

    rt.shutdown_background();
    Ok(())
}

// ─── Reducer ─────────────────────────────────────────────────────────────────

/// Central reducer: receives raw Twitch events + UI commands, tokenizes
/// messages using the emote index, and forwards AppEvents to the UI.
async fn reducer_loop(
    mut cmd_rx: mpsc::Receiver<AppCommand>,
    mut tw_rx: mpsc::Receiver<TwitchEvent>,
    evt_tx: mpsc::Sender<AppEvent>,
    sess_tx: mpsc::Sender<SessionCommand>,
    emote_index: EmoteIndex,
    emote_cache: Option<EmoteCache>,
    badge_map: BadgeMap,
    settings_store: Option<SettingsStore>,
    saved_token: Option<String>,
) {
    // Track URLs we've already queued for image download
    let mut pending_images: HashSet<String> = HashSet::new();

    // Track authenticated user info for local echo messages
    let mut auth_username: Option<String> = None;
    let mut auth_user_id: Option<String> = None;
    let mut local_msg_id: u64 = 1_000_000; // offset to avoid collisions with session IDs

    // If we have a saved token, validate and auto-login
    if let Some(token) = saved_token {
        info!("Found saved token, attempting auto-login…");
        match validate_token(&token).await {
            Ok(login) => {
                info!("Saved token valid for user: {login}");
                let _ = sess_tx.send(SessionCommand::Authenticate {
                    token,
                    nick: login,
                }).await;
            }
            Err(e) => {
                warn!("Saved token invalid ({e}), starting as anonymous");
                if let Some(store) = &settings_store {
                    let _ = store.delete_token();
                }
            }
        }
    }

    loop {
        tokio::select! {
            // Twitch IRC event
            Some(tw_evt) = tw_rx.recv() => {
                match tw_evt {
                    TwitchEvent::Connected => {
                        let _ = evt_tx.send(AppEvent::ConnectionStateChanged {
                            state: ConnectionState::Connected,
                        }).await;
                    }
                    TwitchEvent::Disconnected => {
                        let _ = evt_tx.send(AppEvent::ConnectionStateChanged {
                            state: ConnectionState::Disconnected,
                        }).await;
                    }
                    TwitchEvent::Reconnecting { attempt } => {
                        let _ = evt_tx.send(AppEvent::ConnectionStateChanged {
                            state: ConnectionState::Reconnecting { attempt },
                        }).await;
                    }
                    TwitchEvent::Error(e) => {
                        let _ = evt_tx.send(AppEvent::ConnectionStateChanged {
                            state: ConnectionState::Error(e),
                        }).await;
                    }
                    TwitchEvent::RoomState { channel, room_id } => {
                        info!("Got room-id {room_id} for #{channel}");
                        // Load channel-specific emotes
                        let idx = emote_index.clone();
                        let cache_clone = emote_cache.clone();
                        let rid = room_id.clone();
                        let ch = channel.to_string();
                        tokio::spawn(async move {
                            load_channel_emotes(&ch, &rid, &idx, &cache_clone).await;
                        });
                        // Load channel-specific badges
                        let bm = badge_map.clone();
                        tokio::spawn(async move {
                            load_channel_badges(&room_id, &bm).await;
                        });
                    }
                    TwitchEvent::Authenticated { username, user_id } => {
                        auth_username = Some(username.clone());
                        auth_user_id = Some(user_id.clone());
                        let _ = evt_tx.send(AppEvent::Authenticated {
                            username,
                            user_id,
                        }).await;
                    }
                    TwitchEvent::ChatMessage(mut msg) => {
                        // Snapshot the emote index
                        let snapshot: std::collections::HashMap<String, EmoteInfo> = {
                            let guard = emote_index.read().unwrap();
                            guard.clone()
                        };

                        msg.spans = crust_core::format::tokenize(
                            &msg.raw_text,
                            msg.flags.is_action,
                            &msg.twitch_emotes,
                            &|code| {
                                snapshot.get(code).map(|info| {
                                    (
                                        info.id.clone(),
                                        info.code.clone(),
                                        info.url_1x.clone(),
                                        info.provider.clone(),
                                        // Prefer 4x, fall back to 2x, for HD tooltip preview
                                        info.url_4x.clone().or_else(|| info.url_2x.clone()),
                                    )
                                })
                            },
                        );

                        // ── Resolve badge image URLs ─────────────────
                        {
                            let bm = badge_map.read().unwrap();
                            for badge in &mut msg.sender.badges {
                                badge.url = bm.get(&(badge.name.clone(), badge.version.clone())).cloned();
                            }
                        }

                        // ── Queue image fetches for emotes/emoji/badges
                        // Only fetch the 1x/normal URL eagerly. HD (url_hd)
                        // is fetched on-demand when the user hovers.
                        for span in &msg.spans {
                            let url = match span {
                                crust_core::Span::Emote { url, .. } => Some(url.clone()),
                                crust_core::Span::Emoji { url, .. } => Some(url.clone()),
                                _ => None,
                            };
                            if let Some(url) = url {
                                if !pending_images.contains(&url) {
                                    pending_images.insert(url.clone());
                                    let evt_tx = evt_tx.clone();
                                    let cache = emote_cache.clone();
                                    tokio::spawn(async move {
                                        fetch_emote_image(&url, &cache, &evt_tx).await;
                                    });
                                }
                            }
                        }
                        // Queue badge image fetches
                        for badge in &msg.sender.badges {
                            if let Some(url) = &badge.url {
                                if !pending_images.contains(url) {
                                    pending_images.insert(url.clone());
                                    let url = url.clone();
                                    let evt_tx = evt_tx.clone();
                                    let cache = emote_cache.clone();
                                    tokio::spawn(async move {
                                        fetch_emote_image(&url, &cache, &evt_tx).await;
                                    });
                                }
                            }
                        }

                        let channel = msg.channel.clone();
                        let _ = evt_tx.send(AppEvent::MessageReceived {
                            channel,
                            message: msg,
                        }).await;
                    }
                    TwitchEvent::MessageDeleted { channel, server_id } => {
                        let _ = evt_tx.send(AppEvent::MessageDeleted { channel, server_id }).await;
                    }
                    TwitchEvent::SystemNotice(notice) => {
                        // If this is a "Joined channel" notice, also emit ChannelJoined
                        if notice.text == "Joined channel" {
                            if let Some(ch) = &notice.channel {
                                let _ = evt_tx.send(AppEvent::ChannelJoined {
                                    channel: ch.clone(),
                                }).await;
                            }
                        }
                        let _ = evt_tx.send(AppEvent::SystemNotice(notice)).await;
                    }
                }
            }

            // UI command
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    AppCommand::JoinChannel { channel } => {
                        info!("Joining #{channel}");
                        let _ = sess_tx.send(SessionCommand::JoinChannel(channel.clone())).await;
                        // Also emit ChannelJoined immediately for UI responsiveness
                        let _ = evt_tx.send(AppEvent::ChannelJoined {
                            channel,
                        }).await;
                    }
                    AppCommand::LeaveChannel { channel } => {
                        info!("Leaving #{channel}");
                        let _ = sess_tx.send(SessionCommand::LeaveChannel(channel.clone())).await;
                        let _ = evt_tx.send(AppEvent::ChannelParted { channel }).await;
                    }
                    AppCommand::LoadChannelEmotes { channel_twitch_id } => {
                        let idx = emote_index.clone();
                        let cache_clone = emote_cache.clone();
                        tokio::spawn(async move {
                            load_channel_emotes(
                                "manual",
                                &channel_twitch_id,
                                &idx,
                                &cache_clone,
                            )
                            .await;
                        });
                    }
                    AppCommand::FetchImage { url } => {
                        // On-demand fetch (e.g. HD emote requested on hover)
                        if !pending_images.contains(&url) {
                            pending_images.insert(url.clone());
                            let evt_tx = evt_tx.clone();
                            let cache = emote_cache.clone();
                            tokio::spawn(async move {
                                fetch_emote_image(&url, &cache, &evt_tx).await;
                            });
                        }
                    }
                    AppCommand::Login { token } => {
                        info!("Login requested, validating token…");
                        let sess_tx = sess_tx.clone();
                        let evt_tx = evt_tx.clone();
                        let token_clone = token.clone();
                        // Validate the token via Twitch API, then authenticate
                        match validate_token(&token).await {
                            Ok(login) => {
                                info!("Token valid for user: {login}");
                                // Save token for future sessions
                                if let Some(store) = &settings_store {
                                    if let Err(e) = store.save_token(&token_clone) {
                                        warn!("Failed to save token: {e}");
                                    }
                                }
                                let _ = sess_tx.send(SessionCommand::Authenticate {
                                    token: token_clone,
                                    nick: login,
                                }).await;
                            }
                            Err(e) => {
                                warn!("Token validation failed: {e}");
                                let _ = evt_tx.send(AppEvent::Error {
                                    context: "Login".into(),
                                    message: format!("Invalid token: {e}"),
                                }).await;
                            }
                        }
                    }
                    AppCommand::Logout => {
                        info!("Logout requested");
                        // Delete saved token
                        if let Some(store) = &settings_store {
                            let _ = store.delete_token();
                        }
                        auth_username = None;
                        auth_user_id = None;
                        let _ = sess_tx.send(SessionCommand::LogoutAndReconnect).await;
                        let _ = evt_tx.send(AppEvent::LoggedOut).await;
                    }
                    AppCommand::SendMessage { channel, text } => {
                        debug!("Sending message to #{channel}: {text}");
                        let _ = sess_tx.send(SessionCommand::SendMessage(channel.clone(), text.clone())).await;

                        // ── Local echo: show the sent message immediately ──
                        if let (Some(uname), Some(uid)) = (&auth_username, &auth_user_id) {
                            local_msg_id += 1;
                            let mut echo = ChatMessage {
                                id: MessageId(local_msg_id),
                                server_id: None,
                                timestamp: Utc::now(),
                                channel: channel.clone(),
                                sender: Sender {
                                    user_id: UserId(uid.clone()),
                                    login: uname.to_lowercase(),
                                    display_name: uname.clone(),
                                    color: None,
                                    badges: Vec::new(),
                                },
                                raw_text: text.clone(),
                                spans: smallvec::SmallVec::new(),
                                twitch_emotes: Vec::new(),
                                flags: MessageFlags {
                                    is_action: false,
                                    is_highlighted: false,
                                    is_deleted: false,
                                    is_first_msg: false,
                                    is_self: true,
                                    custom_reward_id: None,
                                },
                            };

                            // Tokenize the echo message
                            let snapshot: std::collections::HashMap<String, EmoteInfo> = {
                                let guard = emote_index.read().unwrap();
                                guard.clone()
                            };
                            echo.spans = crust_core::format::tokenize(
                                &echo.raw_text,
                                false,
                                &echo.twitch_emotes,
                                &|code| {
                                    snapshot.get(code).map(|info| {
                                        (
                                            info.id.clone(),
                                            info.code.clone(),
                                            info.url_1x.clone(),
                                            info.provider.clone(),
                                            info.url_4x.clone().or_else(|| info.url_2x.clone()),
                                        )
                                    })
                                },
                            );

                            // Queue image fetches for emotes in the echo
                            for span in &echo.spans {
                                let url = match span {
                                    crust_core::Span::Emote { url, .. } => Some(url.clone()),
                                    crust_core::Span::Emoji { url, .. } => Some(url.clone()),
                                    _ => None,
                                };
                                if let Some(url) = url {
                                    if !pending_images.contains(&url) {
                                        pending_images.insert(url.clone());
                                        let evt_tx = evt_tx.clone();
                                        let cache = emote_cache.clone();
                                        tokio::spawn(async move {
                                            fetch_emote_image(&url, &cache, &evt_tx).await;
                                        });
                                    }
                                }
                            }

                            let _ = evt_tx.send(AppEvent::MessageReceived {
                                channel,
                                message: echo,
                            }).await;
                        }
                    }
                }
            }

            else => break,
        }
    }

    info!("Reducer loop exiting");
}

// ─── Emote loading ───────────────────────────────────────────────────────────

/// Load global emotes from BTTV, FFZ, 7TV and register in the shared index.
async fn load_global_emotes(index: &EmoteIndex, cache: &Option<EmoteCache>) {
    info!("Loading global emotes…");

    let bttv = BttvProvider::new();
    let ffz = FfzProvider::new();
    let stv = SevenTvProvider::new();

    let (b, f, s) = tokio::join!(bttv.load_global(), ffz.load_global(), stv.load_global());

    let total = b.len() + f.len() + s.len();
    info!("Loaded {total} global emotes (BTTV={}, FFZ={}, 7TV={})", b.len(), f.len(), s.len());

    // Insert in priority order: FFZ < BTTV < 7TV (later overwrites earlier)
    {
        let mut idx = index.write().unwrap();
        for e in f {
            idx.insert(e.code.clone(), e);
        }
        for e in b {
            idx.insert(e.code.clone(), e);
        }
        for e in s {
            idx.insert(e.code.clone(), e);
        }
    }

    // Also register with EmoteCache if available
    if let Some(cache) = cache {
        let idx = index.read().unwrap();
        let emotes: Vec<EmoteInfo> = idx.values().cloned().collect();
        drop(idx);
        cache.register(emotes);
    }
}

/// Load channel-specific emotes from BTTV, FFZ, 7TV.
async fn load_channel_emotes(
    channel_name: &str,
    room_id: &str,
    index: &EmoteIndex,
    cache: &Option<EmoteCache>,
) {
    info!("Loading channel emotes for #{channel_name} (room-id {room_id})");

    let bttv = BttvProvider::new();
    let ffz = FfzProvider::new();
    let stv = SevenTvProvider::new();

    let (b, f, s) = tokio::join!(
        bttv.load_channel(room_id),
        ffz.load_channel(room_id),
        stv.load_channel(room_id),
    );

    let total = b.len() + f.len() + s.len();
    if total == 0 {
        warn!("No channel emotes found for #{channel_name}");
        return;
    }
    info!(
        "Loaded {total} channel emotes for #{channel_name} (BTTV={}, FFZ={}, 7TV={})",
        b.len(),
        f.len(),
        s.len()
    );

    {
        let mut idx = index.write().unwrap();
        for e in f {
            idx.insert(e.code.clone(), e);
        }
        for e in b {
            idx.insert(e.code.clone(), e);
        }
        for e in s {
            idx.insert(e.code.clone(), e);
        }
    }

    if let Some(cache) = cache {
        let idx = index.read().unwrap();
        let emotes: Vec<EmoteInfo> = idx.values().cloned().collect();
        drop(idx);
        cache.register(emotes);
    }
}

// ─── Badge loading ───────────────────────────────────────────────────────────

/// Parse IVR badge response (flat JSON array) and insert into the badge map.
fn parse_ivr_badge_response(
    body: &str,
    map: &mut std::collections::HashMap<(String, String), String>,
) {
    #[derive(serde::Deserialize)]
    struct Version {
        id: String,
        image_url_1x: String,
    }
    #[derive(serde::Deserialize)]
    struct BadgeSet {
        set_id: String,
        versions: Vec<Version>,
    }

    // IVR returns a flat array: [{set_id, versions: [...]}]
    if let Ok(sets) = serde_json::from_str::<Vec<BadgeSet>>(body) {
        for set in sets {
            for ver in set.versions {
                map.insert((set.set_id.clone(), ver.id), ver.image_url_1x);
            }
        }
    }
}

/// Load global Twitch badges via IVR API (no auth required).
async fn load_global_badges(badge_map: &BadgeMap) {
    let client = reqwest::Client::new();
    let url = "https://api.ivr.fi/v2/twitch/badges/global";
    match client.get(url).send().await {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(text) = resp.text().await {
                let mut map = badge_map.write().unwrap();
                let before = map.len();
                parse_ivr_badge_response(&text, &mut map);
                info!("Loaded {} global badges via IVR", map.len() - before);
            }
        }
        Ok(resp) => warn!("IVR global badges returned HTTP {}", resp.status()),
        Err(e) => warn!("Failed to load global badges: {e}"),
    }
}

/// Load channel-specific Twitch badges via IVR API (no auth required).
async fn load_channel_badges(room_id: &str, badge_map: &BadgeMap) {
    let client = reqwest::Client::new();
    let url = format!("https://api.ivr.fi/v2/twitch/badges/channel?id={room_id}");
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(text) = resp.text().await {
                let mut map = badge_map.write().unwrap();
                let before = map.len();
                parse_ivr_badge_response(&text, &mut map);
                info!(
                    "Loaded {} channel badges for room {room_id} via IVR",
                    map.len() - before
                );
            }
        }
        Ok(resp) => warn!("IVR channel badges returned HTTP {} for room {room_id}", resp.status()),
        Err(e) => warn!("Failed to load channel badges for room {room_id}: {e}"),
    }
}

/// Fetch a single emote/emoji/badge image and send raw bytes to UI.
async fn fetch_emote_image(
    url: &str,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let result = if let Some(cache) = cache {
        cache.fetch_and_decode(url).await
    } else {
        fetch_and_decode_raw(url).await
    };

    match result {
        Ok((width, height, raw_bytes)) => {
            let _ = evt_tx
                .send(AppEvent::EmoteImageReady {
                    uri: url.to_owned(),
                    width,
                    height,
                    raw_bytes,
                })
                .await;
        }
        Err(e) => {
            debug!("Failed to fetch emote image {url}: {e}");
        }
    }
}

async fn fetch_and_decode_raw(url: &str) -> Result<(u32, u32, Vec<u8>), crust_emotes::EmoteError> {
    let client = reqwest::Client::new();
    let resp = client.get(url).send().await?;
    let raw = resp.bytes().await?;
    let raw_vec = raw.to_vec();
    // Read dimensions from header only — no full RGBA decode needed
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(&raw_vec))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
        .unwrap_or((1, 1));
    Ok((w, h, raw_vec))
}

// ─── Token validation ────────────────────────────────────────────────────────

/// Validate a Twitch OAuth token via the Twitch API and return the login name.
async fn validate_token(token: &str) -> Result<String, String> {
    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    let client = reqwest::Client::new();
    let resp = client
        .get("https://id.twitch.tv/oauth2/validate")
        .header("Authorization", format!("OAuth {bare}"))
        .send()
        .await
        .map_err(|e| format!("HTTP error: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("Token rejected (HTTP {})", resp.status()));
    }

    #[derive(serde::Deserialize)]
    struct ValidateResponse {
        login: String,
    }

    let body = resp
        .json::<ValidateResponse>()
        .await
        .map_err(|e| format!("Failed to parse validation response: {e}"))?;

    Ok(body.login)
}

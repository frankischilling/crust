use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use chrono::Utc;
use crust_core::events::{AppCommand, AppEvent, ConnectionState};
use crust_core::model::{
    Badge, ChannelId, ChatMessage, EmoteCatalogEntry, MessageFlags, MessageId, MsgKind, Sender, UserId,
    UserProfile,
};
use crust_emotes::{
    cache::EmoteCache,
    providers::{BttvProvider, EmoteInfo, FfzProvider, SevenTvProvider},
    EmoteProvider,
};
use crust_storage::{AppSettings, SettingsStore};
use crust_twitch::{parse_line, parse_privmsg_irc, session::client::{SessionCommand, TwitchEvent, TwitchSession}};
use crust_ui::CrustApp;

const CMD_CHANNEL_SIZE: usize = 128;
const EVT_CHANNEL_SIZE: usize = 4096;
const TWITCH_EVT_SIZE: usize = 4096;

/// Counter for assigning unique IDs to history messages loaded from external APIs.
/// Starts at u64::MAX/2 so it never clashes with live session IDs (which count up from 0).
static HISTORY_MSG_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(u64::MAX / 2);

/// Shared emote index: code → EmoteInfo.
type EmoteIndex = Arc<RwLock<std::collections::HashMap<String, EmoteInfo>>>;

/// Shared badge map: (set_name, version) → image URL.
type BadgeMap = Arc<RwLock<std::collections::HashMap<(String, String), String>>>;



fn main() -> Result<()> {
    // SIGPIPE: handle broken pipe signals on Wayland
    // On Wayland, when a protocol socket (compositor, XWayland, portal)
    // dies, writes produce SIGPIPE. With Rust edition 2021 the default
    // disposition is SIG_DFL → terminate.  Ignore it so the IO layer
    // returns EPIPE normally and libraries can handle the error.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    // Wayland compatibility: handle XDG Settings Portal and clipboard issues
    // On pure-Wayland sessions the XDG Settings Portal may not be running.
    // sctk-adwaita (client-side decorations in winit) queries it for the
    // color-scheme and may time out.
    //
    // A more severe issue: arboard (system clipboard, pulled in by
    // egui-winit) tries the Wayland data-control protocol first. If the
    // compositor doesn't implement it, arboard falls back to X11 clipboard
    // via XWayland. That X11 worker thread can crash when the XWayland
    // connection is closed or times out, which takes down the entire
    // winit event loop ("Io error: Broken pipe" → Exit Failure: 1).
    //
    // Fix: on Wayland, clear DISPLAY so arboard never attempts the X11
    // fallback. The window itself is rendered via Wayland — DISPLAY is
    // only needed for XWayland clipboard, which is the thing crashing.
    // Clipboard copy/paste may not work if the compositor lacks
    // data-control, but at least the app stays alive.
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        // Only clear DISPLAY if arboard's Wayland clipboard is likely to
        // fail (we can't easily probe the protocol list, so we preemptively
        // remove the X11 fallback — the worst outcome is no system
        // clipboard, which is better than an instant crash).
        if std::env::var("DISPLAY").is_ok() {
            std::env::remove_var("DISPLAY");
        }
    }

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

    // Track which emote codes are global (vs channel-specific)
    let global_emote_codes: Arc<RwLock<std::collections::HashSet<String>>> =
        Arc::new(RwLock::new(std::collections::HashSet::new()));

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
        let etx = evt_tx.clone();
        let gc = global_emote_codes.clone();
        async move {
            load_global_emotes(&idx, &cache, &etx, &gc).await;
        }
    });

    // Load global badges in background
    rt.spawn({
        let bm = badge_map.clone();
        let etx = evt_tx.clone();
        let cache = emote_cache.clone();
        async move {
            load_global_badges(&bm, &cache, &etx).await;
        }
    });

    // Spawn the reducer (bridges twitch events → tokenized AppEvents for UI)
    rt.spawn({
        let idx = emote_index.clone();
        let cache = emote_cache.clone();
        let bm = badge_map.clone();
        let gc = global_emote_codes.clone();
        reducer_loop(cmd_rx, tw_evt_rx, evt_tx, sess_cmd_tx, idx, cache, bm, gc, settings_store, saved_token)
    });

    // eframe / egui: UI framework initialization
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

// Reducer: application state reducer

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
    global_emote_codes: GlobalCodes,
    settings_store: Option<SettingsStore>,
    saved_token: Option<String>,
) {
    // Track URLs we've already queued for image download
    let mut pending_images: HashSet<String> = HashSet::new();
    // Track URLs we've already kicked off a link-preview fetch for.
    let mut pending_link_previews: HashSet<String> = HashSet::new();

    // Track authenticated user info for local echo messages
    let mut auth_username: Option<String> = None;
    let mut auth_user_id: Option<String> = None;
    let mut local_msg_id: u64 = 1_000_000; // offset to avoid collisions with session IDs

    // Per-channel cache of the logged-in user's badges + color (from USERSTATE).
    let mut self_badges: HashMap<ChannelId, Vec<Badge>> = HashMap::new();
    let mut self_color: Option<String> = None;

    // Load persisted settings; track which channels are joined so we can
    // keep auto_join up to date and restore them after reconnects.
    let mut settings: AppSettings = settings_store.as_ref()
        .map(|s| s.load())
        .unwrap_or_default();
    let mut joined_channels: HashSet<String> = settings.auto_join.iter().cloned().collect();

    /// Persist the current `joined_channels` set back to disk.
    fn save_channels(store: &Option<SettingsStore>, settings: &mut AppSettings, channels: &HashSet<String>) {
        settings.auto_join = channels.iter().cloned().collect();
        settings.auto_join.sort();
        if let Some(s) = store {
            if let Err(e) = s.save(settings) {
                tracing::warn!("Failed to save settings: {e}");
            }
        }
    }

    // If we have a saved token AND a saved username, immediately tell the UI
    // the user is logged in (optimistic) so they don't see the login prompt.
    // We will validate the token in the background; if invalid we undo it.
    if let (Some(token), username) = (&saved_token, &settings.username) {
        if !token.is_empty() && !username.is_empty() {
            let _ = evt_tx.send(AppEvent::Authenticated {
                username: username.clone(),
                user_id: String::new(), // filled in properly when GLOBALUSERSTATE arrives
            }).await;
        }
    }

    // If we have a saved token, validate and auto-login
    if let Some(token) = saved_token {
        info!("Found saved token, attempting auto-login…");
        match validate_token(&token).await {
            Ok(login) => {
                info!("Saved token valid for user: {login}");
                // Update saved username in case it changed.
                if settings.username != login {
                    settings.username = login.clone();
                    if let Some(store) = &settings_store {
                        let _ = store.save(&settings);
                    }
                }
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
                // Undo the optimistic login we sent earlier.
                let _ = evt_tx.send(AppEvent::LoggedOut).await;
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
                        // Re-join all saved channels after every (re)connect.
                        // Use the sorted auto_join Vec so channel tabs always
                        // open in a stable order (first entry becomes active).
                        // Emit ChannelJoined proactively so the tab appears
                        // immediately without waiting for the IRC confirmation.
                        for ch in &settings.auto_join {
                            let id = ChannelId(ch.clone());
                            let _ = evt_tx.send(AppEvent::ChannelJoined { channel: id.clone() }).await;
                            let _ = sess_tx.send(SessionCommand::JoinChannel(id)).await;
                        }
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
                        let etx = evt_tx.clone();
                        let gc = global_emote_codes.clone();
                        tokio::spawn(async move {
                            load_channel_emotes(&ch, &rid, &idx, &cache_clone, &etx, &gc).await;
                        });
                        // Load channel-specific badges
                        let bm = badge_map.clone();
                        let etx = evt_tx.clone();
                        let cache_b = emote_cache.clone();
                        tokio::spawn(async move {
                            load_channel_badges(&room_id, &bm, &cache_b, &etx).await;
                        });
                        // Load recent chat history
                        let ch_hist = channel.clone();
                        let uname_hist = auth_username.clone();
                        let idx_hist = emote_index.clone();
                        let bm_hist = badge_map.clone();
                        let cache_hist = emote_cache.clone();
                        let etx_hist = evt_tx.clone();
                        tokio::spawn(async move {
                            load_recent_messages(
                                ch_hist.as_str(),
                                uname_hist.as_deref(),
                                &idx_hist,
                                &bm_hist,
                                &cache_hist,
                                &etx_hist,
                            ).await;
                        });
                    }
                    TwitchEvent::Authenticated { username, user_id } => {
                        auth_username = Some(username.clone());
                        auth_user_id = Some(user_id.clone());
                        let _ = evt_tx.send(AppEvent::Authenticated {
                            username,
                            user_id: user_id.clone(),
                        }).await;
                        // Load the authenticated user's personal 7TV emote set
                        let uid = user_id.clone();
                        let idx2 = emote_index.clone();
                        let cache2 = emote_cache.clone();
                        let etx2 = evt_tx.clone();
                        let gc2 = global_emote_codes.clone();
                        tokio::spawn(async move {
                            load_personal_7tv_emotes(&uid, &idx2, &cache2, &etx2, &gc2).await;
                        });
                        // Fetch the user's own avatar for the top-bar profile pill.
                        {
                            let login = auth_username.clone().unwrap_or_default().to_lowercase();
                            let etx3 = evt_tx.clone();
                            tokio::spawn(async move {
                                fetch_self_avatar(&login, etx3).await;
                            });
                        }
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

                        // Resolve badge image URLs
                        {
                            let bm = badge_map.read().unwrap();
                            for badge in &mut msg.sender.badges {
                                badge.url = bm.get(&(badge.name.clone(), badge.version.clone())).cloned();
                            }
                        }

                        // Mention / reply-to-me detection
                        if let Some(ref uname) = auth_username {
                            let uname_lower = uname.to_lowercase();
                            // Direct @mention in message body
                            let has_mention = msg.raw_text
                                .to_lowercase()
                                .contains(&format!("@{uname_lower}"));
                            // Reply directed at us
                            let is_reply_to_me = msg.reply.as_ref()
                                .map(|r| r.parent_user_login.to_lowercase() == uname_lower)
                                .unwrap_or(false);
                            msg.flags.is_mention = has_mention || is_reply_to_me;
                        }

                        // Queue image fetches for emotes/emoji/badges
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
                        // "Joined channel" / "Left channel" are IRC-level confirmations
                        // that fire once per (re)connect attempt, causing duplicate
                        // messages whenever auth triggers a reconnect.  The channel
                        // tab appearing/disappearing is sufficient UI feedback, so we
                        // skip injecting these into the chat feed entirely.
                        let is_join_noise = matches!(
                            notice.text.as_str(),
                            "Joined channel" | "Left channel"
                        );
                        if !is_join_noise {
                            if let Some(ch) = notice.channel.clone() {
                                let msg = make_system_message(
                                    local_msg_id, ch, notice.text.clone(), notice.timestamp,
                                    MsgKind::SystemInfo,
                                );
                                local_msg_id += 1;
                                let _ = evt_tx.send(AppEvent::MessageReceived {
                                    channel: msg.channel.clone(),
                                    message: msg,
                                }).await;
                            }
                        }
                    }
                    TwitchEvent::UserTimedOut { channel, login, seconds } => {
                        // Mark all visible messages from that user as deleted
                        let _ = evt_tx.send(AppEvent::UserMessagesCleared {
                            channel: channel.clone(),
                            login: login.clone(),
                        }).await;
                        // Show a moderation notice in chat
                        let text = format_timeout_text(&login, seconds);
                        let msg = make_system_message(
                            local_msg_id, channel.clone(), text, Utc::now(),
                            MsgKind::Timeout { login: login.clone(), seconds },
                        );
                        local_msg_id += 1;
                        let _ = evt_tx.send(AppEvent::MessageReceived {
                            channel,
                            message: msg,
                        }).await;
                    }
                    TwitchEvent::UserBanned { channel, login } => {
                        let _ = evt_tx.send(AppEvent::UserMessagesCleared {
                            channel: channel.clone(),
                            login: login.clone(),
                        }).await;
                        let text = format!("{login} was permanently banned.");
                        let msg = make_system_message(
                            local_msg_id, channel.clone(), text, Utc::now(),
                            MsgKind::Ban { login: login.clone() },
                        );
                        local_msg_id += 1;
                        let _ = evt_tx.send(AppEvent::MessageReceived {
                            channel,
                            message: msg,
                        }).await;
                    }
                    TwitchEvent::ChatCleared { channel } => {
                        let msg = make_system_message(
                            local_msg_id, channel.clone(),
                            "Chat was cleared by a moderator.".to_owned(),
                            Utc::now(), MsgKind::ChatCleared,
                        );
                        local_msg_id += 1;
                        let _ = evt_tx.send(AppEvent::MessageReceived {
                            channel,
                            message: msg,
                        }).await;
                    }
                    TwitchEvent::SubAlert { channel, display_name, months, plan, is_gift, sub_msg } => {
                        let text = build_sub_text(&display_name, months, &plan, is_gift);
                        let msg = make_system_message(
                            local_msg_id, channel.clone(), text, Utc::now(),
                            MsgKind::Sub { display_name, months, plan, is_gift, sub_msg },
                        );
                        local_msg_id += 1;
                        let _ = evt_tx.send(AppEvent::MessageReceived {
                            channel,
                            message: msg,
                        }).await;
                    }
                    TwitchEvent::Raid { channel, display_name, viewer_count } => {
                        let text = format!("{display_name} is raiding with {viewer_count} viewers!");
                        let msg = make_system_message(
                            local_msg_id, channel.clone(), text, Utc::now(),
                            MsgKind::Raid { display_name, viewer_count },
                        );
                        local_msg_id += 1;
                        let _ = evt_tx.send(AppEvent::MessageReceived {
                            channel,
                            message: msg,
                        }).await;
                    }
                    TwitchEvent::UserStateUpdated { channel, is_mod, mut badges, color } => {
                        // Resolve badge image URLs
                        {
                            let bm = badge_map.read().unwrap();
                            for badge in &mut badges {
                                badge.url = bm.get(&(badge.name.clone(), badge.version.clone())).cloned();
                            }
                        }
                        // Cache for local echo messages
                        self_badges.insert(channel.clone(), badges.clone());
                        if color.is_some() {
                            self_color = color.clone();
                        }
                        let _ = evt_tx.send(AppEvent::UserStateUpdated { channel, is_mod, badges, color }).await;
                    }
                }
            }

            // UI command
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    AppCommand::JoinChannel { channel } => {
                        info!("Joining #{channel}");
                        let _ = sess_tx.send(SessionCommand::JoinChannel(channel.clone())).await;
                        // Emit ChannelJoined immediately for UI responsiveness.
                        let _ = evt_tx.send(AppEvent::ChannelJoined {
                            channel: channel.clone(),
                        }).await;
                        // Inject a single join confirmation into the feed.
                        let join_msg = make_system_message(
                            local_msg_id,
                            channel.clone(),
                            format!("Joined #{}", channel.as_str()),
                            chrono::Utc::now(),
                            MsgKind::SystemInfo,
                        );
                        local_msg_id += 1;
                        let _ = evt_tx.send(AppEvent::MessageReceived {
                            channel: join_msg.channel.clone(),
                            message: join_msg,
                        }).await;
                        // Persist to settings
                        joined_channels.insert(channel.0.to_lowercase());
                        save_channels(&settings_store, &mut settings, &joined_channels);
                    }
                    AppCommand::LeaveChannel { channel } => {
                        info!("Leaving #{channel}");
                        let _ = sess_tx.send(SessionCommand::LeaveChannel(channel.clone())).await;
                        let _ = evt_tx.send(AppEvent::ChannelParted { channel: channel.clone() }).await;
                        // Persist to settings
                        joined_channels.remove(&channel.0.to_lowercase());
                        save_channels(&settings_store, &mut settings, &joined_channels);
                    }
                    AppCommand::LoadChannelEmotes { channel_twitch_id } => {
                        let idx = emote_index.clone();
                        let cache_clone = emote_cache.clone();
                        let etx = evt_tx.clone();
                        let gc = global_emote_codes.clone();
                        tokio::spawn(async move {
                            load_channel_emotes(
                                "manual",
                                &channel_twitch_id,
                                &idx,
                                &cache_clone,
                                &etx,
                                &gc,
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
                    AppCommand::FetchLinkPreview { url } => {
                        if !pending_link_previews.contains(&url) {
                            pending_link_previews.insert(url.clone());
                            let evt_tx = evt_tx.clone();
                            let cache = emote_cache.clone();
                            tokio::spawn(async move {
                                fetch_link_preview(&url, &cache, &evt_tx).await;
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
                                // Set both username and token on the in-memory settings
                                // struct, then do ONE save so neither field overwrites
                                // the other.
                                settings.username = login.clone();
                                settings.oauth_token = token_clone.clone();
                                if let Some(store) = &settings_store {
                                    if let Err(e) = store.save(&settings) {
                                        warn!("Failed to save settings: {e}");
                                    }
                                    // Best-effort keyring (non-fatal).
                                    store.try_save_keyring(&token_clone);
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
                        // Delete saved token and clear saved username
                        if let Some(store) = &settings_store {
                            let _ = store.delete_token();
                        }
                        settings.username = String::new();
                        settings.oauth_token = String::new();
                        if let Some(store) = &settings_store {
                            let _ = store.save(&settings);
                        }
                        auth_username = None;
                        auth_user_id = None;
                        let _ = sess_tx.send(SessionCommand::LogoutAndReconnect).await;
                        let _ = evt_tx.send(AppEvent::LoggedOut).await;
                    }
                    AppCommand::SendMessage { channel, text, reply_to_msg_id } => {
                        debug!("Sending message to #{channel}: {text}");
                        let _ = sess_tx.send(SessionCommand::SendMessage(channel.clone(), text.clone(), reply_to_msg_id)).await;

                        // Local echo: show the sent message immediately
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
                                    color: self_color.clone(),
                                    badges: self_badges.get(&channel).cloned().unwrap_or_default(),
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
                                    is_mention: false,
                                    custom_reward_id: None,
                                    is_history: false,
                                },
                                reply: None,
                                msg_kind: MsgKind::Chat,
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
                    AppCommand::FetchUserProfile { login } => {
                        let etx = evt_tx.clone();
                        tokio::spawn(async move { fetch_user_profile(&login, etx).await; });
                    }
                    AppCommand::TimeoutUser { channel, login, seconds } => {
                        let cmd = format!("/timeout {login} {seconds}");
                        let _ = sess_tx.send(SessionCommand::SendMessage(channel, cmd, None)).await;
                    }
                    AppCommand::BanUser { channel, login } => {
                        let cmd = format!("/ban {login}");
                        let _ = sess_tx.send(SessionCommand::SendMessage(channel, cmd, None)).await;
                    }
                    AppCommand::ClearLocalMessages { channel } => {
                        let _ = evt_tx.send(AppEvent::ChannelMessagesCleared { channel }).await;
                    }
                    AppCommand::OpenUrl { url } => {
                        // Platform-agnostic browser open via xdg-open / open / start.
                        open_url_in_browser(&url);
                    }
                    AppCommand::InjectLocalMessage { channel, text } => {
                        local_msg_id += 1;
                        let msg = make_system_message(
                            local_msg_id, channel.clone(), text, Utc::now(),
                            MsgKind::SystemInfo,
                        );
                        let _ = evt_tx.send(AppEvent::MessageReceived {
                            channel,
                            message: msg,
                        }).await;
                    }
                    AppCommand::ShowUserCard { login, .. } => {
                        // Piggyback on existing FetchUserProfile; the UI
                        // side already handles set_loading via ShowUserCard.
                        let etx = evt_tx.clone();
                        tokio::spawn(async move { fetch_user_profile(&login, etx).await; });
                    }
                }
            }

            else => break,
        }
    }

    info!("Reducer loop exiting");
}

// Emote loading

/// Load global emotes from BTTV, FFZ, 7TV and register in the shared index.
type GlobalCodes = Arc<RwLock<std::collections::HashSet<String>>>;

async fn load_global_emotes(
    index: &EmoteIndex,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
    global_codes: &GlobalCodes,
) {
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

    // Record global codes
    {
        let idx = index.read().unwrap();
        let mut gc = global_codes.write().unwrap();
        for code in idx.keys() {
            gc.insert(code.clone());
        }
    }

    // Send catalog snapshot to the UI
    send_emote_catalog(index, evt_tx, global_codes).await;

    // Eagerly prefetch all emote images so they're ready for the picker / autocomplete
    prefetch_all_emote_images(index, cache, evt_tx);
}

/// Load channel-specific emotes from BTTV, FFZ, 7TV.
/// Load the authenticated viewer's personal 7TV emote set and merge into the global index.
async fn load_personal_7tv_emotes(
    user_id: &str,
    index: &EmoteIndex,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
    global_codes: &GlobalCodes,
) {
    info!("Loading personal 7TV emotes for user-id {user_id}");
    let stv = SevenTvProvider::new();
    let emotes = stv.load_channel(user_id).await;
    if emotes.is_empty() {
        info!("No personal 7TV emotes found for user-id {user_id}");
        return;
    }
    info!("Loaded {} personal 7TV emotes for user-id {user_id}", emotes.len());
    {
        let mut idx = index.write().unwrap();
        for e in emotes {
            idx.insert(e.code.clone(), e);
        }
    }
    send_emote_catalog(index, evt_tx, global_codes).await;
    prefetch_all_emote_images(index, cache, evt_tx);
}

async fn load_channel_emotes(
    channel_name: &str,
    room_id: &str,
    index: &EmoteIndex,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
    global_codes: &GlobalCodes,
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

    // Send catalog snapshot to the UI
    send_emote_catalog(index, evt_tx, global_codes).await;

    // Eagerly prefetch all emote images so they're ready for the picker / autocomplete
    prefetch_all_emote_images(index, cache, evt_tx);
}

/// Build a catalog snapshot from the emote index and send it to the UI.
async fn send_emote_catalog(
    index: &EmoteIndex,
    evt_tx: &mpsc::Sender<AppEvent>,
    global_codes: &GlobalCodes,
) {
    let entries: Vec<EmoteCatalogEntry> = {
        let idx = index.read().unwrap();
        let gc = global_codes.read().unwrap();
        idx.values()
            .map(|e| {
                let scope = if gc.contains(&e.code) {
                    "global"
                } else {
                    "channel"
                };
                EmoteCatalogEntry {
                    code: e.code.clone(),
                    provider: e.provider.clone(),
                    url: e.url_1x.clone(),
                    scope: scope.to_owned(),
                }
            })
            .collect()
    };
    let _ = evt_tx.send(AppEvent::EmoteCatalogUpdated { emotes: entries }).await;
}

/// Eagerly prefetch all emote images in the background so they're available
/// in the emote picker and `:` autocomplete without waiting for lazy fetch.
fn prefetch_all_emote_images(
    index: &EmoteIndex,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let urls: Vec<String> = {
        let idx = index.read().unwrap();
        idx.values().map(|e| e.url_1x.clone()).collect()
    };

    info!("Prefetching {} emote images…", urls.len());

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

// Badge loading
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
async fn load_global_badges(
    badge_map: &BadgeMap,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let client = reqwest::Client::new();
    let url = "https://api.ivr.fi/v2/twitch/badges/global";
    match client.get(url).send().await {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(text) = resp.text().await {
                let new_urls = {
                    let mut map = badge_map.write().unwrap();
                    let before: std::collections::HashSet<String> = map.values().cloned().collect();
                    parse_ivr_badge_response(&text, &mut map);
                    let after_count = map.len();
                    let new: Vec<String> = map.values()
                        .filter(|u| !before.contains(*u))
                        .cloned()
                        .collect();
                    info!("Loaded {} global badges via IVR", after_count - before.len());
                    new
                };
                prefetch_badge_images(new_urls, cache, evt_tx);
            }
        }
        Ok(resp) => warn!("IVR global badges returned HTTP {}", resp.status()),
        Err(e) => warn!("Failed to load global badges: {e}"),
    }
}

/// Load channel-specific Twitch badges via IVR API (no auth required).
async fn load_channel_badges(
    room_id: &str,
    badge_map: &BadgeMap,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let client = reqwest::Client::new();
    let url = format!("https://api.ivr.fi/v2/twitch/badges/channel?id={room_id}");
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(text) = resp.text().await {
                let new_urls = {
                    let mut map = badge_map.write().unwrap();
                    let before: std::collections::HashSet<String> = map.values().cloned().collect();
                    parse_ivr_badge_response(&text, &mut map);
                    let new: Vec<String> = map.values()
                        .filter(|u| !before.contains(*u))
                        .cloned()
                        .collect();
                    info!(
                        "Loaded {} channel badges for room {room_id} via IVR",
                        new.len()
                    );
                    new
                };
                prefetch_badge_images(new_urls, cache, evt_tx);
            }
        }
        Ok(resp) => warn!("IVR channel badges returned HTTP {} for room {room_id}", resp.status()),
        Err(e) => warn!("Failed to load channel badges for room {room_id}: {e}"),
    }
}

/// Eagerly prefetch a list of badge image URLs so they appear instantly
/// in messages without waiting for the first chat message to trigger a fetch.
fn prefetch_badge_images(
    urls: Vec<String>,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    if urls.is_empty() { return; }
    info!("Prefetching {} badge images…", urls.len());
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

// Recent message history

/// Fetch recent messages for a channel and send `AppEvent::HistoryLoaded`.
/// Primary source: recent-messages.robotty.de (covers all channels, correct
/// path uses a hyphen: /recent-messages/).  Fallback: logs.ivr.fi (large
/// channels only, returns objects with a "raw" IRC line field, newest-first).
async fn load_recent_messages(
    channel: &str,
    local_nick: Option<&str>,
    emote_index: &EmoteIndex,
    badge_map: &BadgeMap,
    emote_cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let ch = channel.trim_start_matches('#');

    // NOTE: the correct path is /recent-messages/ (hyphen), not /recent_messages/.
    let robotty_url = format!(
        "https://recent-messages.robotty.de/api/v2/recent-messages/{ch}?limit=800"
    );
    let ivr_url = format!(
        "https://logs.ivr.fi/channel/{ch}?json=1&reverse=true&limit=800"
    );

    let client = reqwest::Client::new();

    // Try robotty first; it covers all channels (including small ones).
    // Fall back to IVR if robotty fails or returns nothing.
    let raw_lines: Vec<String> = 'fetch: {
        if let Ok(resp) = client.get(&robotty_url)
            .header("Accept", "application/json")
            .send().await
        {
            if resp.status().is_success() {
                if let Ok(text) = resp.text().await {
                    #[derive(serde::Deserialize)]
                    struct RobottyResponse { messages: Vec<String> }
                    if let Ok(p) = serde_json::from_str::<RobottyResponse>(&text) {
                        if !p.messages.is_empty() {
                            break 'fetch p.messages;
                        }
                    }
                }
            }
        }
        // IVR fallback
        match client.get(&ivr_url)
            .header("Accept", "application/json")
            .send().await
        {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(text) = resp.text().await {
                    #[derive(serde::Deserialize)]
                    struct IvrMsg { raw: String }
                    #[derive(serde::Deserialize)]
                    struct IvrResp { messages: Vec<IvrMsg> }
                    if let Ok(mut p) = serde_json::from_str::<IvrResp>(&text) {
                        p.messages.reverse(); // IVR is newest-first
                        break 'fetch p.messages.into_iter().map(|m| m.raw).collect();
                    }
                }
                Vec::new()
            }
            Ok(resp) => { warn!("chat-history: both sources failed for #{ch} (IVR HTTP {})", resp.status()); Vec::new() }
            Err(e)   => { warn!("chat-history: both sources failed for #{ch}: {e}"); Vec::new() }
        }
    };

    if raw_lines.is_empty() { return; }

    let emote_snapshot: std::collections::HashMap<String, EmoteInfo> = {
        let guard = emote_index.read().unwrap();
        guard.clone()
    };

    let mut messages: Vec<ChatMessage> = Vec::with_capacity(raw_lines.len());

    for line in &raw_lines {
        let irc_msg = match parse_line(line) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if irc_msg.command != "PRIVMSG" { continue; }

        let id = HISTORY_MSG_ID.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        let mut msg = match parse_privmsg_irc(&irc_msg, local_nick, id) {
            Some(m) => m,
            None => continue,
        };

        // Tokenize
        msg.spans = crust_core::format::tokenize(
            &msg.raw_text,
            msg.flags.is_action,
            &msg.twitch_emotes,
            &|code| {
                emote_snapshot.get(code).map(|info| {
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

        // Resolve badge URLs
        {
            let bm = badge_map.read().unwrap();
            for badge in &mut msg.sender.badges {
                badge.url = bm.get(&(badge.name.clone(), badge.version.clone())).cloned();
            }
        }

        // Mention detection
        if let Some(nick) = local_nick {
            let nick_lower = nick.to_lowercase();
            let has_mention = msg.raw_text.to_lowercase().contains(&format!("@{nick_lower}"));
            let is_reply_to_me = msg.reply.as_ref()
                .map(|r| r.parent_user_login.to_lowercase() == nick_lower)
                .unwrap_or(false);
            msg.flags.is_mention = has_mention || is_reply_to_me;
        }

        // Queue image fetches for new emotes/badges
        for span in &msg.spans {
            let img_url = match span {
                crust_core::Span::Emote { url, .. } => Some(url.clone()),
                crust_core::Span::Emoji { url, .. } => Some(url.clone()),
                _ => None,
            };
            if let Some(img_url) = img_url {
                let evt_tx = evt_tx.clone();
                let cache = emote_cache.clone();
                tokio::spawn(async move {
                    fetch_emote_image(&img_url, &cache, &evt_tx).await;
                });
            }
        }
        for badge in &msg.sender.badges {
            if let Some(badge_url) = &badge.url {
                let badge_url = badge_url.clone();
                let evt_tx = evt_tx.clone();
                let cache = emote_cache.clone();
                tokio::spawn(async move {
                    fetch_emote_image(&badge_url, &cache, &evt_tx).await;
                });
            }
        }

        messages.push(msg);
    }

    if messages.is_empty() { return; }

    info!("Loaded {} historical messages for #{ch}", messages.len());
    let channel_id = crust_core::model::ChannelId::new(ch);
    let _ = evt_tx.send(AppEvent::HistoryLoaded { channel: channel_id, messages }).await;
}

// Token validation

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

// User profile

/// Fetch a Twitch user profile from the IVR API (no auth required) and send
/// `AppEvent::UserProfileLoaded`.  Also pre-fetches avatar bytes so the popup
/// can show the real avatar immediately.
async fn fetch_user_profile(login: &str, evt_tx: mpsc::Sender<AppEvent>) {
    #[derive(serde::Deserialize)]
    struct IvrRoles {
        #[serde(rename = "isPartner", default)]
        is_partner: bool,
        #[serde(rename = "isAffiliate", default)]
        is_affiliate: bool,
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
    }

    let url = format!("https://api.ivr.fi/v2/twitch/user?login={login}");
    let client = reqwest::Client::new();
    let resp = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => { warn!("IVR user fetch returned HTTP {} for {login}", r.status()); return; }
        Err(e) => { warn!("IVR user fetch failed for {login}: {e}"); return; }
    };

    let users: Vec<IvrUser> = match resp.json().await {
        Ok(u) => u,
        Err(e) => { warn!("IVR user response parse failed for {login}: {e}"); return; }
    };

    let Some(user) = users.into_iter().next() else {
        warn!("IVR returned no user for {login}");
        return;
    };

    let avatar_url = user.logo.clone();

    let profile = UserProfile {
        id: user.id,
        login: user.login,
        display_name: user.display_name,
        description: user.description,
        created_at: user.created_at,
        avatar_url: avatar_url.clone(),
        followers: user.followers,
        is_partner: user.roles.as_ref().map_or(false, |r| r.is_partner),
        is_affiliate: user.roles.as_ref().map_or(false, |r| r.is_affiliate),
    };

    // Pre-fetch avatar bytes so egui can display them right away.
    if let Some(ref logo) = avatar_url {
        if let Ok((w, h, raw)) = fetch_and_decode_raw(logo).await {
            let _ = evt_tx.send(AppEvent::EmoteImageReady {
                uri: logo.clone(),
                width: w,
                height: h,
                raw_bytes: raw,
            }).await;
        }
    }

    let _ = evt_tx.send(AppEvent::UserProfileLoaded { profile }).await;
}

/// Fetch the logged-in user's avatar URL and image bytes for the top-bar pill.
async fn fetch_self_avatar(login: &str, evt_tx: mpsc::Sender<AppEvent>) {
    if login.is_empty() { return; }

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
    let Some(user) = users.into_iter().next() else { return };
    let Some(avatar_url) = user.logo else { return };

    // Pre-fetch image bytes
    if let Ok((w, h, raw)) = fetch_and_decode_raw(&avatar_url).await {
        let _ = evt_tx.send(AppEvent::EmoteImageReady {
            uri: avatar_url.clone(),
            width: w,
            height: h,
            raw_bytes: raw,
        }).await;
    }

    let _ = evt_tx.send(AppEvent::SelfAvatarLoaded { avatar_url }).await;
}

// System-message helpers

/// Construct a system (non-chat) ChatMessage for inline display in a channel.
fn make_system_message(
    id: u64,
    channel: ChannelId,
    text: String,
    timestamp: chrono::DateTime<Utc>,
    kind: MsgKind,
) -> ChatMessage {
    use smallvec::smallvec;
    let spans = smallvec![crust_core::model::Span::Text { text: text.clone(), is_action: false }];
    ChatMessage {
        id: MessageId(id),
        server_id: None,
        timestamp,
        channel,
        sender: Sender {
            user_id: UserId(String::new()),
            login: String::new(),
            display_name: String::new(),
            color: None,
            badges: Vec::new(),
        },
        raw_text: text,
        spans,
        twitch_emotes: Vec::new(),
        flags: MessageFlags {
            is_action: false,
            is_highlighted: false,
            is_deleted: false,
            is_first_msg: false,
            is_self: false,
            is_mention: false,
            custom_reward_id: None,
            is_history: false,
        },
        reply: None,
        msg_kind: kind,
    }
}

/// Format a timeout notice for display in chat.
fn format_timeout_text(login: &str, seconds: u32) -> String {
    if seconds < 60 {
        format!("{login} was timed out for {seconds}s.")
    } else if seconds < 3600 {
        format!("{login} was timed out for {}m.", seconds / 60)
    } else {
        format!("{login} was timed out for {}h {}m.", seconds / 3600, (seconds % 3600) / 60)
    }
}

/// Build a human-readable sub alert text.
fn build_sub_text(display_name: &str, months: u32, plan: &str, is_gift: bool) -> String {
    if is_gift {
        format!("{display_name} received a gifted {plan} subscription! ({months} months total)")
    } else if months <= 1 {
        format!("{display_name} subscribed with {plan}!")
    } else {
        format!("{display_name} resubscribed with {plan}! ({months} months)")
    }
}
/// Open a URL in the system's default browser.
/// Uses `xdg-open` on Linux, `open` on macOS, `cmd /c start` on Windows.
fn open_url_in_browser(url: &str) {
    #[cfg(target_os = "linux")]
    { let _ = std::process::Command::new("xdg-open").arg(url).spawn(); }
    #[cfg(target_os = "macos")]
    { let _ = std::process::Command::new("open").arg(url).spawn(); }
    #[cfg(target_os = "windows")]
    { let _ = std::process::Command::new("cmd").args(["/c", "start", url]).spawn(); }
}

// Link preview fetch

async fn fetch_link_preview(
    url: &str,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let send_empty = |url: &str| {
        AppEvent::LinkPreviewReady {
            url: url.to_owned(),
            title: None, description: None, thumbnail_url: None,
        }
    };

    let client = match reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; crust-chat/1.0; +https://github.com/crust)")
        .timeout(std::time::Duration::from_secs(6))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => { let _ = evt_tx.send(send_empty(url)).await; return; }
    };

    let resp = match client.get(url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => { let _ = evt_tx.send(send_empty(url)).await; return; }
    };

    // Only parse HTML
    let ct = resp.headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    if !ct.contains("html") {
        let _ = evt_tx.send(send_empty(url)).await;
        return;
    }

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(_) => { let _ = evt_tx.send(send_empty(url)).await; return; }
    };
    // Only read the first 64 KB to avoid processing megabyte HTML files.
    let html = String::from_utf8_lossy(&bytes[..bytes.len().min(65_536)]);

    let title = og_meta(&html, "og:title")
        .or_else(|| og_meta(&html, "twitter:title"))
        .or_else(|| html_title(&html));
    let description = og_meta(&html, "og:description")
        .or_else(|| og_meta(&html, "twitter:description"));
    let thumbnail_url = og_meta(&html, "og:image")
        .or_else(|| og_meta(&html, "twitter:image"));

    // Kick off thumbnail image fetch so bytes land in emote_bytes.
    if let Some(ref img) = thumbnail_url {
        fetch_emote_image(img, cache, evt_tx).await;
    }

    let _ = evt_tx.send(AppEvent::LinkPreviewReady {
        url: url.to_owned(),
        title,
        description,
        thumbnail_url,
    }).await;
}

/// Extract the content of a `<meta property="{prop}" ...>` or `<meta name="{prop}" ...>` tag.
fn og_meta(html: &str, prop: &str) -> Option<String> {
    let prop_lower = prop.to_lowercase();
    let mut offset = 0;
    while let Some(rel) = html[offset..].to_lowercase().find("<meta") {
        let abs = offset + rel;
        let rest = &html[abs..];
        // Find end of this tag
        let tag_end = rest.find('>').unwrap_or(rest.len()).min(512);
        let tag = &rest[..tag_end];
        let tag_lower = tag.to_lowercase();

        let has_prop =
            tag_lower.contains(&format!("property=\"{prop_lower}\"")) ||
            tag_lower.contains(&format!("property='{prop_lower}'")) ||
            tag_lower.contains(&format!("name=\"{prop_lower}\"")) ||
            tag_lower.contains(&format!("name='{prop_lower}'"));

        if has_prop {
            if let Some(val) = html_attr(tag, "content") {
                return Some(html_entities(val));
            }
        }
        offset = abs + 5;
    }
    None
}

/// Extract an attribute value from an HTML tag snippet.
fn html_attr<'a>(tag: &'a str, attr: &str) -> Option<&'a str> {
    let tag_lower = tag.to_lowercase();
    let needle = format!("{}=", attr.to_lowercase());
    let pos = tag_lower.find(&needle)?;
    let after = &tag[pos + needle.len()..];
    if after.starts_with('"') {
        let end = after[1..].find('"')?;
        Some(&after[1..1 + end])
    } else if after.starts_with('\'') {
        let end = after[1..].find('\'')?;
        Some(&after[1..1 + end])
    } else {
        None
    }
}

/// Extract `<title>` text as a fallback.
fn html_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let s = lower.find("<title")? + 6;
    let tag_end = lower[s..].find('>')?;
    let body_start = s + tag_end + 1;
    let body_end = lower[body_start..].find("</title>")?;
    let text = html[body_start..body_start + body_end].trim().to_owned();
    if text.is_empty() { None } else { Some(html_entities(&text)) }
}

/// Decode common HTML entities.
fn html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
     .replace("&lt;", "<")
     .replace("&gt;", ">")
     .replace("&quot;", "\"")
     .replace("&#39;", "'")
     .replace("&apos;", "'")
     .replace("&nbsp;", " ")
}
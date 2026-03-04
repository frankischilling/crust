use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use chrono::Utc;
use crust_core::events::{AppCommand, AppEvent, ConnectionState};
use crust_core::model::{
    Badge, ChannelId, ChatMessage, EmoteCatalogEntry, MessageFlags, MessageId, MsgKind, Sender,
    SenderPaint, SenderPaintStop, UserId, UserProfile,
};
use crust_emotes::{
    cache::EmoteCache,
    providers::{BttvProvider, EmoteInfo, FfzProvider, KickProvider, SevenTvProvider},
    EmoteProvider,
};
use crust_kick::session::{KickEvent, KickSession, KickSessionCommand};
use crust_storage::{AppSettings, SettingsStore};
use crust_twitch::session::generic_irc::{
    is_raw_irc_protocol_line, GenericIrcEvent, GenericIrcSession, GenericIrcSessionCommand,
};
use crust_twitch::{
    parse_line, parse_privmsg_irc,
    session::client::{SessionCommand, TwitchEvent, TwitchSession},
};
use crust_ui::CrustApp;

const CMD_CHANNEL_SIZE: usize = 128;
const EVT_CHANNEL_SIZE: usize = 4096;
const TWITCH_EVT_SIZE: usize = 4096;
const KICK_EVT_SIZE: usize = 4096;
const IRC_EVT_SIZE: usize = 4096;
const TWITCH_MAX_MESSAGE_CHARS: usize = 500;
const SEVENTV_GQL_URL: &str = "https://api.7tv.app/v3/gql";

/// Counter for assigning unique IDs to history messages loaded from external APIs.
/// Starts at u64::MAX/2 so it never clashes with live session IDs (which count up from 0).
static HISTORY_MSG_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(u64::MAX / 2);

/// Shared emote index: "provider:code" → EmoteInfo.
/// Keyed by compound key so that emotes with the same code from different
/// providers are all preserved (important for the emote picker catalog).
type EmoteIndex = Arc<RwLock<std::collections::HashMap<String, EmoteInfo>>>;

/// Build the compound key used in [`EmoteIndex`].
#[inline]
fn emote_key(provider: &str, code: &str) -> String {
    format!("{provider}:{code}")
}

/// Resolve an emote by code across all providers.
/// Priority: 7TV > BTTV > FFZ > Kick (matches visual override order).
fn resolve_emote<'a>(
    idx: &'a std::collections::HashMap<String, EmoteInfo>,
    code: &str,
) -> Option<&'a EmoteInfo> {
    for provider in &["7tv", "bttv", "ffz", "kick"] {
        if let Some(info) = idx.get(&emote_key(provider, code)) {
            return Some(info);
        }
    }
    None
}

/// Shared badge map: (set_name, version) → image URL.
type BadgeMap = Arc<RwLock<std::collections::HashMap<(String, String), String>>>;

#[derive(Debug, Clone, Default)]
struct SevenTvUserStyleRaw {
    color: Option<i32>,
    paint_id: Option<String>,
    badge_id: Option<String>,
}

#[derive(Debug, Clone)]
struct SevenTvBadgeMeta {
    tooltip: Option<String>,
    url: String,
}

#[derive(Debug, Clone, Default)]
struct SevenTvResolvedStyle {
    color_hex: Option<String>,
    paint: Option<SenderPaint>,
    badge: Option<Badge>,
}

enum SevenTvCosmeticUpdate {
    Catalog {
        paints: HashMap<String, SenderPaint>,
        badges: HashMap<String, SevenTvBadgeMeta>,
    },
    UserStyle {
        twitch_user_id: String,
        style: Option<SevenTvUserStyleRaw>,
    },
    /// Batch of Twitch user-ids discovered in history messages that need
    /// their 7TV styles resolved.
    BatchUserLookup {
        user_ids: Vec<String>,
    },
}

#[derive(Debug, serde::Deserialize)]
struct SevenTvGraphQlResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<SevenTvGraphQlError>,
}

#[derive(Debug, serde::Deserialize)]
struct SevenTvGraphQlError {
    message: String,
}

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
    // fallback. The window itself is rendered via Wayland - DISPLAY is
    // only needed for XWayland clipboard, which is the thing crashing.
    // Clipboard copy/paste may not work if the compositor lacks
    // data-control, but at least the app stays alive.
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        // Only clear DISPLAY if arboard's Wayland clipboard is likely to
        // fail (we can't easily probe the protocol list, so we preemptively
        // remove the X11 fallback - the worst outcome is no system
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

    // Kick session channels
    let (kick_evt_tx, kick_evt_rx) = mpsc::channel::<KickEvent>(KICK_EVT_SIZE);
    let (kick_cmd_tx, kick_cmd_rx) = mpsc::channel::<KickSessionCommand>(64);

    // Generic IRC session channels
    let (irc_evt_tx, irc_evt_rx) = mpsc::channel::<GenericIrcEvent>(IRC_EVT_SIZE);
    let (irc_cmd_tx, irc_cmd_rx) = mpsc::channel::<GenericIrcSessionCommand>(128);

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
    let initial_settings: AppSettings = settings_store
        .as_ref()
        .map(|s| s.load())
        .unwrap_or_default();
    let kick_runtime_enabled = initial_settings.enable_kick_beta;
    let irc_runtime_enabled = initial_settings.enable_irc_beta;
    // Determine which account to auto-login as on startup:
    // prefer the explicitly pinned default_account, fall back to the last
    // active username, and finally fall back to the legacy single token.
    let saved_token = settings_store.as_ref().and_then(|s| {
        let startup_user = if !initial_settings.default_account.is_empty() {
            initial_settings.default_account.clone()
        } else {
            initial_settings.username.clone()
        };
        if !startup_user.is_empty() {
            s.load_account_token(&startup_user)
                .or_else(|| s.load_token())
        } else {
            s.load_token()
        }
    });

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

    // Spawn Kick Pusher session (beta, opt-in).
    if kick_runtime_enabled {
        rt.spawn({
            let session = KickSession::new(kick_evt_tx, kick_cmd_rx);
            session.run()
        });
    }

    // Spawn generic IRC session manager (beta, opt-in).
    if irc_runtime_enabled {
        rt.spawn({
            let session = GenericIrcSession::new(irc_evt_tx, irc_cmd_rx);
            session.run()
        });
    }

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

    // Spawn the reducer (bridges twitch/kick events → tokenized AppEvents for UI)
    rt.spawn({
        let idx = emote_index.clone();
        let cache = emote_cache.clone();
        let bm = badge_map.clone();
        let gc = global_emote_codes.clone();
        reducer_loop(
            cmd_rx,
            tw_evt_rx,
            kick_evt_rx,
            irc_evt_rx,
            evt_tx,
            sess_cmd_tx,
            kick_cmd_tx,
            irc_cmd_tx,
            idx,
            cache,
            bm,
            gc,
            settings_store,
            saved_token,
            kick_runtime_enabled,
            irc_runtime_enabled,
        )
    });

    // eframe / egui: UI framework initialization
    let native_opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Crust – Twitch, Kick & IRC Chat")
            .with_inner_size([1100.0, 700.0])
            .with_min_inner_size([300.0, 200.0])
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

/// Result of a background token validation, sent back to the reducer loop.
enum TokenValidationResult {
    Startup {
        token: String,
        result: Result<ValidateInfo, ValidateError>,
    },
    Login {
        token: String,
        result: Result<ValidateInfo, ValidateError>,
    },
    AddAccount {
        token: String,
        result: Result<ValidateInfo, ValidateError>,
    },
}

/// Central reducer: receives raw Twitch/Kick events + UI commands, tokenizes
/// messages using the emote index, and forwards AppEvents to the UI.
async fn reducer_loop(
    mut cmd_rx: mpsc::Receiver<AppCommand>,
    mut tw_rx: mpsc::Receiver<TwitchEvent>,
    mut kick_rx: mpsc::Receiver<KickEvent>,
    mut irc_rx: mpsc::Receiver<GenericIrcEvent>,
    evt_tx: mpsc::Sender<AppEvent>,
    sess_tx: mpsc::Sender<SessionCommand>,
    kick_tx: mpsc::Sender<KickSessionCommand>,
    irc_tx: mpsc::Sender<GenericIrcSessionCommand>,
    emote_index: EmoteIndex,
    emote_cache: Option<EmoteCache>,
    badge_map: BadgeMap,
    global_emote_codes: GlobalCodes,
    settings_store: Option<SettingsStore>,
    saved_token: Option<String>,
    kick_runtime_enabled: bool,
    irc_runtime_enabled: bool,
) {
    // Track URLs we've already queued for image download
    let mut pending_images: HashSet<String> = HashSet::new();
    // Track URLs we've already kicked off a link-preview fetch for.
    let mut pending_link_previews: HashSet<String> = HashSet::new();

    // 7TV cosmetics cache: global paints/badges + per-user resolved styles.
    let (stv_update_tx, mut stv_update_rx) = mpsc::channel::<SevenTvCosmeticUpdate>(512);
    let mut stv_paints: HashMap<String, SenderPaint> = HashMap::new();
    let mut stv_badges: HashMap<String, SevenTvBadgeMeta> = HashMap::new();
    let mut stv_user_styles_raw: HashMap<String, SevenTvUserStyleRaw> = HashMap::new();
    let mut stv_user_styles_resolved: HashMap<String, SevenTvResolvedStyle> = HashMap::new();
    let mut stv_pending_user_lookups: HashSet<String> = HashSet::new();

    // Shared HTTP client for all 7TV API calls (connection pooling + HTTP/2).
    let stv_http_client: reqwest::Client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    // Limit concurrent 7TV user-style lookups to avoid overwhelming the API.
    let stv_lookup_sem = Arc::new(tokio::sync::Semaphore::new(20));

    {
        let tx = stv_update_tx.clone();
        let client = stv_http_client.clone();
        tokio::spawn(async move {
            if let Some((paints, badges)) = load_7tv_cosmetics_catalog(&client).await {
                let _ = tx
                    .send(SevenTvCosmeticUpdate::Catalog { paints, badges })
                    .await;
            }
        });
    }

    // Track authenticated user info for local echo messages
    let mut auth_username: Option<String> = None;
    let mut auth_user_id: Option<String> = None;
    let mut local_msg_id: u64 = 1_000_000; // offset to avoid collisions with session IDs
                                           // Helix API credentials extracted from the validate response.
                                           // Required for moderation calls (timeout / ban) via POST /helix/moderation/bans.
    let mut helix_client_id: Option<String> = None;
    // Room-ids for every joined channel (broadcaster_id used by Helix API).
    let mut channel_room_ids: std::collections::HashMap<ChannelId, String> =
        std::collections::HashMap::new();

    // Per-channel cache of the logged-in user's badges + color (from USERSTATE).
    let mut self_badges: HashMap<ChannelId, Vec<Badge>> = HashMap::new();
    let mut self_color: Option<String> = None;

    // Load persisted settings; track which channels are joined so we can
    // keep auto_join up to date and restore them after reconnects.
    let mut settings: AppSettings = settings_store
        .as_ref()
        .map(|s| s.load())
        .unwrap_or_default();
    let mut kick_beta_enabled = settings.enable_kick_beta;
    let mut irc_beta_enabled = settings.enable_irc_beta;
    fn parse_saved_channel(raw: &str) -> Option<ChannelId> {
        let id = ChannelId::parse_user_input(raw)?;
        if id.display_name().is_empty() {
            None
        } else {
            Some(id)
        }
    }

    fn parsed_auto_join_channels(settings: &AppSettings) -> Vec<ChannelId> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for raw in &settings.auto_join {
            if let Some(id) = parse_saved_channel(raw) {
                if seen.insert(id.0.clone()) {
                    out.push(id);
                }
            } else {
                warn!("Ignoring invalid auto_join entry: {:?}", raw);
            }
        }
        out
    }

    let parsed_initial_channels = parsed_auto_join_channels(&settings);
    let mut canonical_auto_join: Vec<String> = parsed_initial_channels
        .iter()
        .map(|id| id.0.clone())
        .collect();
    canonical_auto_join.sort();
    let mut existing_auto_join = settings.auto_join.clone();
    existing_auto_join.sort();

    // Rewrite auto-join on startup when invalid/legacy entries are present.
    if canonical_auto_join != existing_auto_join {
        info!(
            "Sanitizing auto-join entries: {} -> {}",
            existing_auto_join.len(),
            canonical_auto_join.len()
        );
        settings.auto_join = canonical_auto_join.clone();
        if let Some(store) = &settings_store {
            if let Err(e) = store.save(&settings) {
                warn!("Failed to save sanitized auto-join settings: {e}");
            }
        }
    }

    let mut joined_channels: HashSet<String> = canonical_auto_join.into_iter().collect();

    // Set to `true` whenever we explicitly send `SessionCommand::Authenticate`.
    // While this flag is set, `TwitchEvent::Connected` will skip re-joining
    // channels because the connection is either anonymous (about to be
    // replaced) or mid-reconnect.  The channels are joined instead from
    // `TwitchEvent::Authenticated` once the correct identity is confirmed.
    // For passive reconnects (network drop on an already-authenticated
    // session) this flag stays `false`, so `Connected` handles the rejoin
    // as before.
    let mut auth_in_progress = false;

    // Internal channel for background token validation results so
    // validate_token() never blocks the reducer loop.
    let (token_val_tx, mut token_val_rx) = mpsc::channel::<TokenValidationResult>(8);

    /// Persist the current `joined_channels` set back to disk.
    fn save_channels(
        store: &Option<SettingsStore>,
        settings: &mut AppSettings,
        channels: &HashSet<String>,
    ) {
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
    let startup_username = if !settings.default_account.is_empty() {
        settings.default_account.clone()
    } else {
        settings.username.clone()
    };
    if let (Some(token), uname) = (&saved_token, &startup_username) {
        if !token.is_empty() && !uname.is_empty() {
            let _ = evt_tx
                .send(AppEvent::Authenticated {
                    username: uname.to_string(),
                    user_id: String::new(), // filled in properly when GLOBALUSERSTATE arrives
                })
                .await;
        }
    }

    // If we have a saved token, validate in the background and auto-login
    // via the token_val_rx channel.  Set auth_in_progress *now* so that
    // Connected events arriving before validation completes don't trigger
    // premature channel rejoins.
    if let Some(token) = saved_token {
        info!("Found saved token, spawning background validation…");
        auth_in_progress = true;
        let tx = token_val_tx.clone();
        tokio::spawn(async move {
            let result = validate_token(&token).await;
            let _ = tx.send(TokenValidationResult::Startup { token, result }).await;
        });
    }

    // Broadcast the initial account list so the UI knows about all saved
    // accounts immediately (e.g. after auto-login on startup).
    {
        let account_names: Vec<String> = settings
            .accounts
            .iter()
            .map(|a| a.username.clone())
            .collect();
        let active = if settings.username.is_empty() {
            None
        } else {
            Some(settings.username.clone())
        };
        let default = if settings.default_account.is_empty() {
            None
        } else {
            Some(settings.default_account.clone())
        };
        let _ = evt_tx
            .send(AppEvent::AccountListUpdated {
                accounts: account_names,
                active,
                default,
            })
            .await;
    }

    let _ = evt_tx
        .send(AppEvent::BetaFeaturesUpdated {
            kick_enabled: kick_beta_enabled,
            irc_enabled: irc_beta_enabled,
            irc_nickserv_user: settings.irc_nickserv_user.clone(),
            irc_nickserv_pass: settings.irc_nickserv_pass.clone(),
            always_on_top: settings.always_on_top,
        })
        .await;

    // Configure preferred IRC nickname (if set).
    if irc_runtime_enabled && irc_beta_enabled && !settings.irc_nick.trim().is_empty() {
        let _ = irc_tx
            .send(GenericIrcSessionCommand::SetNick(
                settings.irc_nick.trim().to_owned(),
            ))
            .await;
    }

    // Configure NickServ auto-identify credentials (if set).
    if irc_runtime_enabled
        && irc_beta_enabled
        && !settings.irc_nickserv_user.trim().is_empty()
        && !settings.irc_nickserv_pass.trim().is_empty()
    {
        let _ = irc_tx
            .send(GenericIrcSessionCommand::SetNickServAuth {
                nickserv_user: settings.irc_nickserv_user.trim().to_owned(),
                nickserv_pass: settings.irc_nickserv_pass.trim().to_owned(),
            })
            .await;
    }

    // Restore IRC channels immediately. Generic IRC connections are opened
    // lazily per-server as soon as join commands are sent.
    if irc_runtime_enabled && irc_beta_enabled {
        let irc_restore: Vec<ChannelId> = parsed_auto_join_channels(&settings)
            .into_iter()
            .filter(|id| id.is_irc())
            .collect();
        if !irc_restore.is_empty() {
            info!(
                "Restoring {} IRC channels from auto-join",
                irc_restore.len()
            );
        }
        for id in irc_restore {
            let _ = evt_tx
                .send(AppEvent::ChannelJoined {
                    channel: id.clone(),
                })
                .await;
            let _ = irc_tx
                .send(GenericIrcSessionCommand::JoinChannel {
                    channel: id,
                    key: None,
                })
                .await;
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
                        // Re-join channels only for passive reconnects (network
                        // drops).  When auth_in_progress is true, the connection
                        // is either anonymous or mid-auth-switch; joining channels
                        // would be premature and creates a double-join (the real
                        // rejoin happens from TwitchEvent::Authenticated below).
                        if !auth_in_progress {
                            let twitch_restore: Vec<ChannelId> = parsed_auto_join_channels(&settings)
                                .into_iter()
                                .filter(|id| id.is_twitch())
                                .collect();
                            if !twitch_restore.is_empty() {
                                info!("Restoring {} Twitch channels from auto-join", twitch_restore.len());
                            }
                            for id in twitch_restore {
                                let _ = evt_tx.send(AppEvent::ChannelJoined { channel: id.clone() }).await;
                                let _ = sess_tx.send(SessionCommand::JoinChannel(id)).await;
                            }
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
                            state: ConnectionState::Error(e.clone()),
                        }).await;
                        // Also surface as a chat-visible error so the user
                        // doesn't have to notice the subtle status-bar change.
                        let _ = evt_tx.send(AppEvent::Error {
                            context: "Connection".into(),
                            message: e,
                        }).await;
                    }
                    TwitchEvent::RoomState { channel, room_id, emote_only, followers_only, slow, subs_only, r9k } => {
                        info!("Got room-id {room_id} for #{channel}");
                        channel_room_ids.insert(channel.clone(), room_id.clone());

                        // Forward room-mode updates to the UI.
                        let _ = evt_tx.send(AppEvent::RoomStateUpdated {
                            channel: channel.clone(),
                            emote_only,
                            followers_only,
                            slow,
                            subs_only,
                            r9k,
                        }).await;

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
                        let stv_tx_hist = stv_update_tx.clone();
                        tokio::spawn(async move {
                            load_recent_messages(
                                ch_hist.as_str(),
                                uname_hist.as_deref(),
                                &idx_hist,
                                &bm_hist,
                                &cache_hist,
                                &etx_hist,
                                &stv_tx_hist,
                            ).await;
                        });
                    }
                    TwitchEvent::Authenticated { username, user_id } => {
                        // If we got here because of an explicit Authenticate
                        // command (auth_in_progress = true), we need to rejoin
                        // channels now that the correct identity is confirmed.
                        // For passive reconnects the flag is false and channels
                        // were already re-joined from TwitchEvent::Connected.
                        let join_now = auth_in_progress;
                        auth_in_progress = false;

                        auth_username = Some(username.clone());
                        auth_user_id = Some(user_id.clone());
                        let _ = evt_tx.send(AppEvent::Authenticated {
                            username,
                            user_id: user_id.clone(),
                        }).await;

                        if join_now {
                            let twitch_restore: Vec<ChannelId> = parsed_auto_join_channels(&settings)
                                .into_iter()
                                .filter(|id| id.is_twitch())
                                .collect();
                            if !twitch_restore.is_empty() {
                                info!("Restoring {} Twitch channels after authentication", twitch_restore.len());
                            }
                            for id in twitch_restore {
                                let _ = evt_tx.send(AppEvent::ChannelJoined { channel: id.clone() }).await;
                                let _ = sess_tx.send(SessionCommand::JoinChannel(id)).await;
                            }
                        }

                        // Load the authenticated user's personal 7TV emote set
                        let uid = user_id.clone();
                        let idx2 = emote_index.clone();
                        let cache2 = emote_cache.clone();
                        let etx2 = evt_tx.clone();
                        let gc2 = global_emote_codes.clone();
                        tokio::spawn(async move {
                            load_personal_7tv_emotes(&uid, &idx2, &cache2, &etx2, &gc2).await;
                        });

                        // Prime cosmetics cache for the logged-in user.
                        if !user_id.trim().is_empty()
                            && stv_pending_user_lookups.insert(user_id.clone())
                        {
                            let tx = stv_update_tx.clone();
                            let client = stv_http_client.clone();
                            let sem = stv_lookup_sem.clone();
                            tokio::spawn(async move {
                                let _permit = sem.acquire().await;
                                let style = load_7tv_user_style_for_twitch(&client, &user_id).await;
                                let _ = tx
                                    .send(SevenTvCosmeticUpdate::UserStyle {
                                        twitch_user_id: user_id,
                                        style,
                                    })
                                    .await;
                            });
                        }
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
                        // Hold read lock only during tokenization (no clone needed)
                        {
                            let emote_guard = emote_index.read().unwrap();
                            msg.spans = crust_core::format::tokenize(
                                &msg.raw_text,
                                msg.flags.is_action,
                                &msg.twitch_emotes,
                                &|code| {
                                    resolve_emote(&emote_guard, code).map(|info| {
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
                        }

                        // Resolve badge image URLs
                        {
                            let bm = badge_map.read().unwrap();
                            for badge in &mut msg.sender.badges {
                                badge.url = resolve_badge_url(&bm, &badge.name, &badge.version);
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

                        // Apply cached 7TV cosmetics for Twitch users and
                        // queue a background style lookup when missing.
                        if msg.channel.is_twitch() {
                            let twitch_user_id = msg.sender.user_id.0.trim().to_owned();
                            if !twitch_user_id.is_empty() {
                                if let Some(style) = stv_user_styles_resolved.get(&twitch_user_id)
                                {
                                    apply_7tv_cosmetics_to_sender(&mut msg.sender, style);
                                } else if stv_pending_user_lookups.insert(twitch_user_id.clone())
                                {
                                    let tx = stv_update_tx.clone();
                                    let client = stv_http_client.clone();
                                    let sem = stv_lookup_sem.clone();
                                    tokio::spawn(async move {
                                        let _permit = sem.acquire().await;
                                        let style =
                                            load_7tv_user_style_for_twitch(&client, &twitch_user_id).await;
                                        let _ = tx
                                            .send(SevenTvCosmeticUpdate::UserStyle {
                                                twitch_user_id,
                                                style,
                                            })
                                            .await;
                                    });
                                }
                            }
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
                        let gifted_to_me = is_gift
                            && auth_username
                                .as_deref()
                                .map(|u| u.eq_ignore_ascii_case(display_name.as_str()))
                                .unwrap_or(false);
                        let text = build_sub_text(&display_name, months, &plan, is_gift);
                        let mut msg = make_system_message(
                            local_msg_id, channel.clone(), text, Utc::now(),
                            MsgKind::Sub { display_name, months, plan, is_gift, sub_msg },
                        );
                        if gifted_to_me {
                            msg.flags.is_mention = true;
                            msg.flags.is_highlighted = true;
                        }
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
                                badge.url = resolve_badge_url(&bm, &badge.name, &badge.version);
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

            // Kick Pusher event
            Some(kick_evt) = kick_rx.recv() => {
                if !kick_runtime_enabled || !kick_beta_enabled {
                    continue;
                }
                match kick_evt {
                    KickEvent::Connected => {
                        info!("Kick session connected");
                        let kick_restore: Vec<ChannelId> = parsed_auto_join_channels(&settings)
                            .into_iter()
                            .filter(|id| id.is_kick())
                            .collect();
                        if !kick_restore.is_empty() {
                            info!("Restoring {} Kick channels from auto-join", kick_restore.len());
                        }
                        for id in kick_restore {
                            let _ = evt_tx.send(AppEvent::ChannelJoined { channel: id.clone() }).await;
                            let _ = kick_tx.send(KickSessionCommand::JoinChannel(id)).await;
                        }
                    }
                    KickEvent::Disconnected => {
                        info!("Kick session disconnected");
                    }
                    KickEvent::Reconnecting { attempt } => {
                        info!("Kick reconnecting (attempt {attempt})");
                    }
                    KickEvent::Error(e) => {
                        warn!("Kick error: {e}");
                        let _ = evt_tx.send(AppEvent::Error {
                            context: "Kick".into(),
                            message: e,
                        }).await;
                    }
                    KickEvent::ChannelInfoResolved { channel, chatroom_id, user_id } => {
                        info!("Kick channel {} resolved: chatroom={chatroom_id}, user={user_id}", channel.display_name());
                        // Load Kick-native + 7TV(Kick) emotes once channel info is resolved.
                        let idx = emote_index.clone();
                        let cache_clone = emote_cache.clone();
                        let etx = evt_tx.clone();
                        let gc = global_emote_codes.clone();
                        let ch = channel.clone();
                        tokio::spawn(async move {
                            load_kick_channel_emotes(&ch, user_id, &idx, &cache_clone, &etx, &gc).await;
                        });
                    }
                    KickEvent::ChatMessage(mut msg) => {
                        let (normalized_text, inline_emotes) =
                            normalize_kick_inline_emotes(&msg.raw_text);
                        if !inline_emotes.is_empty() {
                            msg.raw_text = normalized_text;
                            let mut inserted = 0usize;
                            {
                                let mut idx = emote_index.write().unwrap();
                                for (id, code) in inline_emotes {
                                    let info = EmoteInfo {
                                        id: id.clone(),
                                        code: code.clone(),
                                        url_1x: kick_inline_emote_url(&id),
                                        url_2x: None,
                                        url_4x: None,
                                        provider: "kick".to_owned(),
                                    };
                                    let key = emote_key("kick", &code);
                                    let replace = idx
                                        .get(&key)
                                        .map(|e| e.provider != "kick" || e.id != id)
                                        .unwrap_or(true);
                                    if replace {
                                        idx.insert(key, info);
                                        inserted += 1;
                                    }
                                }
                            }
                            if inserted > 0 {
                                if let Some(cache) = emote_cache.as_ref() {
                                    let idx = emote_index.read().unwrap();
                                    let emotes: Vec<EmoteInfo> = idx.values().cloned().collect();
                                    drop(idx);
                                    cache.register(emotes);
                                }
                            }
                        }

                        // Tokenize for emotes/emoji/URLs
                        {
                            let emote_guard = emote_index.read().unwrap();
                            msg.spans = crust_core::format::tokenize(
                                &msg.raw_text,
                                msg.flags.is_action,
                                &msg.twitch_emotes,
                                &|code| {
                                    resolve_emote(&emote_guard, code).map(|info| {
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
                        }

                        // Queue image fetches for emotes/emoji
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
                    KickEvent::MessageDeleted { channel, server_id } => {
                        let _ = evt_tx.send(AppEvent::MessageDeleted { channel, server_id }).await;
                    }
                    KickEvent::UserBanned { channel, login } => {
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
                    KickEvent::ChatCleared { channel } => {
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
                    KickEvent::SystemNotice(notice) => {
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
            }

            // Generic IRC event
            Some(irc_evt) = irc_rx.recv() => {
                if !irc_runtime_enabled || !irc_beta_enabled {
                    continue;
                }
                match irc_evt {
                    GenericIrcEvent::Connected { server } => {
                        info!("IRC connected: {server}");
                        if let Some(ch) = ChannelId::parse_user_input(&server) {
                            let msg = make_system_message(
                                local_msg_id,
                                ch,
                                format!("IRC connected: {server}"),
                                Utc::now(),
                                MsgKind::SystemInfo,
                            );
                            local_msg_id += 1;
                            let _ = evt_tx
                                .send(AppEvent::MessageReceived {
                                    channel: msg.channel.clone(),
                                    message: msg,
                                })
                                .await;
                        }
                    }
                    GenericIrcEvent::Disconnected { server } => {
                        info!("IRC disconnected: {server}");
                        if let Some(ch) = ChannelId::parse_user_input(&server) {
                            let msg = make_system_message(
                                local_msg_id,
                                ch,
                                format!("IRC disconnected: {server}"),
                                Utc::now(),
                                MsgKind::SystemInfo,
                            );
                            local_msg_id += 1;
                            let _ = evt_tx
                                .send(AppEvent::MessageReceived {
                                    channel: msg.channel.clone(),
                                    message: msg,
                                })
                                .await;
                        }
                    }
                    GenericIrcEvent::Reconnecting { server, attempt } => {
                        info!("IRC reconnecting ({server}) attempt {attempt}");
                        if let Some(ch) = ChannelId::parse_user_input(&server) {
                            let msg = make_system_message(
                                local_msg_id,
                                ch,
                                format!("IRC reconnecting ({server}) attempt {attempt}"),
                                Utc::now(),
                                MsgKind::SystemInfo,
                            );
                            local_msg_id += 1;
                            let _ = evt_tx
                                .send(AppEvent::MessageReceived {
                                    channel: msg.channel.clone(),
                                    message: msg,
                                })
                                .await;
                        }
                    }
                    GenericIrcEvent::Error { server, message } => {
                        warn!("IRC error ({server}): {message}");
                        let _ = evt_tx.send(AppEvent::Error {
                            context: format!("IRC ({server})"),
                            message,
                        }).await;
                    }
                    GenericIrcEvent::ChannelRedirected {
                        server_key: (host, port, tls),
                        old_channel,
                        new_channel,
                    } => {
                        info!(
                            "IRC channel redirect: #{old_channel} → #{new_channel} on {host}:{port}"
                        );
                        let old_id = ChannelId::irc(&host, port, tls, &old_channel);
                        let new_id = ChannelId::irc(&host, port, tls, &new_channel);

                        // Update persisted channel set
                        joined_channels.remove(&old_id.as_str().to_lowercase());
                        joined_channels.insert(new_id.as_str().to_lowercase());
                        save_channels(&settings_store, &mut settings, &joined_channels);

                        // Tell UI to seamlessly replace old tab with new one
                        let _ = evt_tx
                            .send(AppEvent::ChannelRedirected {
                                old_channel: old_id.clone(),
                                new_channel: new_id.clone(),
                            })
                            .await;

                        // System notice in the new channel
                        let redirect_msg = make_system_message(
                            local_msg_id,
                            new_id.clone(),
                            format!(
                                "Redirected from #{old_channel} to #{new_channel}"
                            ),
                            Utc::now(),
                            MsgKind::SystemInfo,
                        );
                        local_msg_id += 1;
                        let _ = evt_tx
                            .send(AppEvent::MessageReceived {
                                channel: new_id,
                                message: redirect_msg,
                            })
                            .await;
                    }
                    GenericIrcEvent::SystemNotice(notice) => {
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
                    GenericIrcEvent::TopicChanged { channel, topic } => {
                        let _ = evt_tx.send(AppEvent::IrcTopicChanged {
                            channel,
                            topic,
                        }).await;
                    }
                    GenericIrcEvent::ChatMessage(mut msg) => {
                        // Tokenize for emotes/emoji/URLs
                        {
                            let emote_guard = emote_index.read().unwrap();
                            msg.spans = crust_core::format::tokenize(
                                &msg.raw_text,
                                msg.flags.is_action,
                                &msg.twitch_emotes,
                                &|code| {
                                    resolve_emote(&emote_guard, code).map(|info| {
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
                        }

                        // Mention detection
                        let mut has_mention = false;
                        if let Some(ref uname) = auth_username {
                            let uname_lower = uname.to_lowercase();
                            has_mention = msg.raw_text
                                .to_lowercase()
                                .contains(&format!("@{uname_lower}"));
                        }
                        // Also match the configured IRC nick as a whole word.
                        if !has_mention {
                            let irc_nick = settings.irc_nick.trim();
                            if !irc_nick.is_empty() {
                                let nick_lower = irc_nick.to_lowercase();
                                let text_lower = msg.raw_text.to_lowercase();
                                has_mention = text_lower
                                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                                    .any(|w| w == nick_lower);
                            }
                        }
                        msg.flags.is_mention = has_mention;

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

                        let channel = msg.channel.clone();
                        let _ = evt_tx.send(AppEvent::MessageReceived {
                            channel,
                            message: msg,
                        }).await;
                    }
                }
            }

            // Internal 7TV cosmetics updates (catalog + per-user style lookups).
            Some(stv_update) = stv_update_rx.recv() => {
                match stv_update {
                    SevenTvCosmeticUpdate::Catalog { paints, badges } => {
                        info!("7TV catalog received: {} paints, {} badges", paints.len(), badges.len());
                        stv_paints = paints;
                        stv_badges = badges;

                        // Re-resolve any styles we already learned before
                        // the cosmetics catalog was available.
                        stv_user_styles_resolved.clear();
                        let mut updates: Vec<(String, SevenTvResolvedStyle)> = Vec::new();
                        for (uid, style) in &stv_user_styles_raw {
                            let resolved = resolve_7tv_user_style(style, &stv_paints, &stv_badges);
                            updates.push((uid.clone(), resolved.clone()));
                            stv_user_styles_resolved.insert(uid.clone(), resolved);
                        }

                        let paint_count = updates.iter().filter(|(_, r)| r.paint.is_some()).count();
                        if !updates.is_empty() {
                            info!("7TV re-resolved {} cached user styles ({} with paints)", updates.len(), paint_count);
                        }

                        for (user_id, resolved) in updates {
                            if user_id.is_empty() {
                                continue;
                            }
                            let _ = evt_tx
                                .send(AppEvent::SenderCosmeticsUpdated {
                                    user_id,
                                    color: resolved.color_hex,
                                    paint: resolved.paint,
                                    badge: resolved.badge,
                                })
                                .await;
                        }
                    }
                    SevenTvCosmeticUpdate::UserStyle {
                        twitch_user_id,
                        style,
                    } => {
                        stv_pending_user_lookups.remove(&twitch_user_id);
                        if let Some(raw) = style {
                            let resolved = resolve_7tv_user_style(&raw, &stv_paints, &stv_badges);
                            let has_paint = resolved.paint.is_some();
                            stv_user_styles_raw.insert(twitch_user_id.clone(), raw);
                            stv_user_styles_resolved
                                .insert(twitch_user_id.clone(), resolved.clone());

                            if has_paint {
                                info!(
                                    "7TV paint resolved for user {}: {}",
                                    twitch_user_id,
                                    resolved.paint.as_ref().map(|p| p.name.as_str()).unwrap_or("?")
                                );
                            }

                            if !twitch_user_id.is_empty() {
                                let _ = evt_tx
                                    .send(AppEvent::SenderCosmeticsUpdated {
                                        user_id: twitch_user_id,
                                        color: resolved.color_hex,
                                        paint: resolved.paint,
                                        badge: resolved.badge,
                                    })
                                    .await;
                            }
                        }
                    }
                    SevenTvCosmeticUpdate::BatchUserLookup { user_ids } => {
                        // Triggered by history loading — queue lookups for
                        // users we haven't seen yet.
                        let mut queued = 0u32;
                        for uid in user_ids {
                            if uid.is_empty() { continue; }
                            if stv_user_styles_resolved.contains_key(&uid) { continue; }
                            if !stv_pending_user_lookups.insert(uid.clone()) { continue; }
                            queued += 1;
                            let tx = stv_update_tx.clone();
                            let client = stv_http_client.clone();
                            let sem = stv_lookup_sem.clone();
                            tokio::spawn(async move {
                                let _permit = sem.acquire().await;
                                let style =
                                    load_7tv_user_style_for_twitch(&client, &uid).await;
                                let _ = tx
                                    .send(SevenTvCosmeticUpdate::UserStyle {
                                        twitch_user_id: uid,
                                        style,
                                    })
                                    .await;
                            });
                        }
                        if queued > 0 {
                            info!("7TV: queued {queued} user-style lookups from history");
                        }
                    }
                }
            }

            // UI command
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    AppCommand::JoinChannel { channel } => {
                        if channel.is_kick() {
                            if !kick_beta_enabled || !kick_runtime_enabled {
                                let _ = evt_tx.send(AppEvent::Error {
                                    context: "Kick".into(),
                                    message: "Kick compatibility is disabled in Settings (beta).".into(),
                                }).await;
                                continue;
                            }
                            info!("Joining Kick channel {}", channel.display_name());
                            let _ = kick_tx.send(KickSessionCommand::JoinChannel(channel.clone())).await;
                        } else if channel.is_irc() {
                            if !irc_beta_enabled || !irc_runtime_enabled {
                                let _ = evt_tx.send(AppEvent::Error {
                                    context: "IRC".into(),
                                    message: "IRC compatibility is disabled in Settings (beta).".into(),
                                }).await;
                                continue;
                            }
                            let label = channel
                                .irc_target()
                                .map(|t| format!("{}:{} #{}", t.host, t.port, t.channel))
                                .unwrap_or_else(|| channel.as_str().to_owned());
                            info!("Joining IRC channel {label}");
                            let _ = irc_tx
                                .send(GenericIrcSessionCommand::JoinChannel {
                                    channel: channel.clone(),
                                    key: None,
                                })
                                .await;
                        } else {
                            info!("Joining #{channel}");
                            let _ = sess_tx.send(SessionCommand::JoinChannel(channel.clone())).await;
                        }
                        // Emit ChannelJoined immediately for UI responsiveness.
                        let _ = evt_tx.send(AppEvent::ChannelJoined {
                            channel: channel.clone(),
                        }).await;
                        // Inject a single join confirmation into the feed.
                        let platform_label = if channel.is_kick() {
                            "Kick"
                        } else if channel.is_irc() {
                            "IRC"
                        } else {
                            "Twitch"
                        };
                        let join_msg = make_system_message(
                            local_msg_id,
                            channel.clone(),
                            format!("Joined {} channel: {}", platform_label, channel.display_name()),
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
                    AppCommand::JoinIrcChannel { channel, key } => {
                        if !channel.is_irc() {
                            continue;
                        }
                        if !irc_beta_enabled || !irc_runtime_enabled {
                            let _ = evt_tx.send(AppEvent::Error {
                                context: "IRC".into(),
                                message: "IRC compatibility is disabled in Settings (beta).".into(),
                            }).await;
                            continue;
                        }
                        let label = channel
                            .irc_target()
                            .map(|t| format!("{}:{} #{}", t.host, t.port, t.channel))
                            .unwrap_or_else(|| channel.as_str().to_owned());
                        info!(
                            "Joining IRC channel {}{}",
                            label,
                            key.as_deref()
                                .filter(|k| !k.is_empty())
                                .map(|_| " (keyed)")
                                .unwrap_or("")
                        );
                        let _ = irc_tx
                            .send(GenericIrcSessionCommand::JoinChannel {
                                channel: channel.clone(),
                                key: key.clone(),
                            })
                            .await;
                        let _ = evt_tx
                            .send(AppEvent::ChannelJoined {
                                channel: channel.clone(),
                            })
                            .await;
                        let join_msg = make_system_message(
                            local_msg_id,
                            channel.clone(),
                            format!("Joined IRC channel: {}", channel.display_name()),
                            chrono::Utc::now(),
                            MsgKind::SystemInfo,
                        );
                        local_msg_id += 1;
                        let _ = evt_tx
                            .send(AppEvent::MessageReceived {
                                channel: join_msg.channel.clone(),
                                message: join_msg,
                            })
                            .await;
                        joined_channels.insert(channel.0.to_lowercase());
                        save_channels(&settings_store, &mut settings, &joined_channels);
                    }
                    AppCommand::LeaveChannel { channel } => {
                        if channel.is_kick() {
                            info!("Leaving Kick channel {}", channel.display_name());
                            let _ = kick_tx.send(KickSessionCommand::LeaveChannel(channel.clone())).await;
                        } else if channel.is_irc() {
                            info!("Leaving IRC channel {}", channel.as_str());
                            let _ = irc_tx.send(GenericIrcSessionCommand::LeaveChannel(channel.clone())).await;
                        } else {
                            info!("Leaving #{channel}");
                            let _ = sess_tx.send(SessionCommand::LeaveChannel(channel.clone())).await;
                        }
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
                        info!("Login requested, spawning background validation…");
                        let tx = token_val_tx.clone();
                        tokio::spawn(async move {
                            let result = validate_token(&token).await;
                            let _ = tx.send(TokenValidationResult::Login { token, result }).await;
                        });
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
                        // Broadcast updated account list.
                        let account_names: Vec<String> = settings.accounts.iter().map(|a| a.username.clone()).collect();
                        let default = if settings.default_account.is_empty() { None } else { Some(settings.default_account.clone()) };
                        let _ = evt_tx.send(AppEvent::AccountListUpdated {
                            accounts: account_names,
                            active: None,
                            default,
                        }).await;
                    }
                    AppCommand::AddAccount { token } => {
                        info!("AddAccount requested, spawning background validation…");
                        let tx = token_val_tx.clone();
                        tokio::spawn(async move {
                            let result = validate_token(&token).await;
                            let _ = tx.send(TokenValidationResult::AddAccount { token, result }).await;
                        });
                    }
                    AppCommand::SwitchAccount { username } => {
                        info!("SwitchAccount to {username}");
                        let token_opt = settings_store.as_ref().and_then(|s| s.load_account_token(&username));
                        if let Some(token) = token_opt {
                            settings.username = username.clone();
                            // Keep the legacy oauth_token field in sync so that
                            // load_token() fallback always returns the right token.
                            settings.oauth_token = token.clone();
                            if let Some(store) = &settings_store {
                                let _ = store.save(&settings);
                                // Refresh per-account keyring slot too.
                                store.try_save_account_keyring(&username, &token);
                            }
                            // Reset auth state before re-authenticating so the
                            // UI clears the old avatar immediately.
                            auth_username = None;
                            auth_user_id = None;
                            let _ = evt_tx.send(AppEvent::LoggedOut).await;
                            auth_in_progress = true;
                            let _ = sess_tx.send(SessionCommand::Authenticate {
                                token,
                                nick: username.clone(),
                            }).await;
                            let account_names: Vec<String> = settings.accounts.iter().map(|a| a.username.clone()).collect();
                            let default = if settings.default_account.is_empty() { None } else { Some(settings.default_account.clone()) };
                            let _ = evt_tx.send(AppEvent::AccountListUpdated {
                                accounts: account_names,
                                active: Some(username),
                                default,
                            }).await;
                        } else {
                            warn!("SwitchAccount: no saved token for {username}");
                            let _ = evt_tx.send(AppEvent::Error {
                                context: "SwitchAccount".into(),
                                message: format!("No saved token for {username}. Please add the account again."),
                            }).await;
                        }
                    }
                    AppCommand::RemoveAccount { username } => {
                        info!("RemoveAccount {username}");
                        let was_active = auth_username.as_deref() == Some(username.as_str());
                        if let Some(store) = &settings_store {
                            let _ = store.delete_account(&username);
                        }
                        settings.accounts.retain(|a| a.username != username);
                        // Clear default_account if the removed account was the default.
                        if settings.default_account == username {
                            settings.default_account = String::new();
                        }
                        if was_active {
                            // Try to switch to the first remaining account.
                            let next = settings.accounts.first().cloned();
                            if let Some(acc) = next {
                                let token_opt = settings_store.as_ref().and_then(|s| s.load_account_token(&acc.username));
                                if let Some(token) = token_opt {
                                    settings.username = acc.username.clone();
                                    auth_in_progress = true;
                                    let _ = sess_tx.send(SessionCommand::Authenticate {
                                        token,
                                        nick: acc.username.clone(),
                                    }).await;
                                } else {
                                    auth_username = None;
                                    auth_user_id = None;
                                    settings.username = String::new();
                                    let _ = sess_tx.send(SessionCommand::LogoutAndReconnect).await;
                                    let _ = evt_tx.send(AppEvent::LoggedOut).await;
                                }
                            } else {
                                auth_username = None;
                                auth_user_id = None;
                                settings.username = String::new();
                                let _ = sess_tx.send(SessionCommand::LogoutAndReconnect).await;
                                let _ = evt_tx.send(AppEvent::LoggedOut).await;
                            }
                            if let Some(store) = &settings_store {
                                let _ = store.save(&settings);
                            }
                        }
                        let account_names: Vec<String> = settings.accounts.iter().map(|a| a.username.clone()).collect();
                        let active = if settings.username.is_empty() { None } else { Some(settings.username.clone()) };
                        let default = if settings.default_account.is_empty() { None } else { Some(settings.default_account.clone()) };
                        let _ = evt_tx.send(AppEvent::AccountListUpdated {
                            accounts: account_names,
                            active,
                            default,
                        }).await;
                    }
                    AppCommand::SetDefaultAccount { username } => {
                        info!("SetDefaultAccount → {}", if username.is_empty() { "(none)" } else { &username });
                        settings.default_account = username.clone();
                        if let Some(store) = &settings_store {
                            let _ = store.save(&settings);
                        }
                        let account_names: Vec<String> = settings.accounts.iter().map(|a| a.username.clone()).collect();
                        let active = if settings.username.is_empty() { None } else { Some(settings.username.clone()) };
                        let default = if username.is_empty() { None } else { Some(username) };
                        let _ = evt_tx.send(AppEvent::AccountListUpdated {
                            accounts: account_names,
                            active,
                            default,
                        }).await;
                    }
                    AppCommand::SetIrcNick { nick } => {
                        let trimmed = nick.trim();
                        if trimmed.is_empty() {
                            let _ = evt_tx.send(AppEvent::Error {
                                context: "IRC".into(),
                                message: "Nickname cannot be empty.".into(),
                            }).await;
                            continue;
                        }
                        settings.irc_nick = trimmed.to_owned();
                        if let Some(store) = &settings_store {
                            if let Err(e) = store.save(&settings) {
                                warn!("Failed to save IRC nickname: {e}");
                            }
                        }
                        if irc_beta_enabled && irc_runtime_enabled {
                            let _ = irc_tx
                                .send(GenericIrcSessionCommand::SetNick(trimmed.to_owned()))
                                .await;
                        }
                    }
                    AppCommand::SetIrcAuth {
                        nickserv_user,
                        nickserv_pass,
                    } => {
                        settings.irc_nickserv_user = nickserv_user.clone();
                        settings.irc_nickserv_pass = nickserv_pass.clone();
                        if let Some(store) = &settings_store {
                            if let Err(e) = store.save(&settings) {
                                warn!("Failed to save IRC NickServ credentials: {e}");
                            }
                        }
                        if irc_beta_enabled && irc_runtime_enabled {
                            let _ = irc_tx
                                .send(GenericIrcSessionCommand::SetNickServAuth {
                                    nickserv_user,
                                    nickserv_pass,
                                })
                                .await;
                        }
                    }
                    AppCommand::SetBetaFeatures {
                        kick_enabled,
                        irc_enabled,
                    } => {
                        kick_beta_enabled = kick_enabled;
                        irc_beta_enabled = irc_enabled;
                        settings.enable_kick_beta = kick_enabled;
                        settings.enable_irc_beta = irc_enabled;
                        if let Some(store) = &settings_store {
                            if let Err(e) = store.save(&settings) {
                                warn!("Failed to save beta feature flags: {e}");
                            }
                        }
                        let _ = evt_tx
                            .send(AppEvent::BetaFeaturesUpdated {
                                kick_enabled,
                                irc_enabled,
                                irc_nickserv_user: settings.irc_nickserv_user.clone(),
                                irc_nickserv_pass: settings.irc_nickserv_pass.clone(),
                                always_on_top: settings.always_on_top,
                            })
                            .await;

                        if (kick_enabled && !kick_runtime_enabled)
                            || (irc_enabled && !irc_runtime_enabled)
                        {
                            let _ = evt_tx.send(AppEvent::Error {
                                context: "Settings".into(),
                                message: "Restart Crust to apply newly enabled beta transports.".into(),
                            }).await;
                        }
                    }
                    AppCommand::SetAlwaysOnTop { enabled } => {
                        settings.always_on_top = enabled;
                        if let Some(store) = &settings_store {
                            if let Err(e) = store.save(&settings) {
                                warn!("Failed to save always-on-top setting: {e}");
                            }
                        }
                    }
                    AppCommand::SendMessage {
                        channel,
                        text,
                        mut reply_to_msg_id,
                        reply,
                    } => {
                        if reply_to_msg_id.is_none() {
                            reply_to_msg_id =
                                reply.as_ref().map(|r| r.parent_msg_id.clone());
                        }

                        debug!("Sending message to #{channel}: {text}");

                        // Twitch hard-limit: reject overlong messages before
                        // they hit the IRC layer (and before local-echo).
                        if channel.is_twitch() {
                            let char_count = text.chars().count();
                            if char_count > TWITCH_MAX_MESSAGE_CHARS {
                                let _ = evt_tx
                                    .send(AppEvent::Error {
                                        context: "Twitch".into(),
                                        message: format!(
                                            "Message too long ({char_count}/{TWITCH_MAX_MESSAGE_CHARS}). Twitch allows up to {TWITCH_MAX_MESSAGE_CHARS} characters.",
                                        ),
                                    })
                                    .await;
                                continue;
                            }
                        }

                        if channel.is_irc_server_tab() {
                            if !irc_beta_enabled || !irc_runtime_enabled {
                                let _ = evt_tx.send(AppEvent::Error {
                                    context: "IRC".into(),
                                    message: "IRC compatibility is disabled in Settings (beta).".into(),
                                }).await;
                                continue;
                            }
                            if text.trim_start().starts_with('/')
                                || is_raw_irc_protocol_line(&text)
                            {
                                // Allow IRC protocol slash commands (e.g. /list, /whois, /raw)
                                // and raw lines (e.g. PRIVMSG #chan :text) directly from
                                // the server tab.
                                let _ = irc_tx
                                    .send(GenericIrcSessionCommand::SendMessage(
                                        channel.clone(),
                                        text.clone(),
                                    ))
                                    .await;
                            } else {
                                let msg = make_system_message(
                                    local_msg_id,
                                    channel.clone(),
                                    "Connected to IRC server. Use /join #channel to enter chat rooms.".to_owned(),
                                    Utc::now(),
                                    MsgKind::SystemInfo,
                                );
                                local_msg_id += 1;
                                let _ = evt_tx.send(AppEvent::MessageReceived {
                                    channel: channel.clone(),
                                    message: msg,
                                }).await;
                            }
                        } else if channel.is_kick() {
                            if !kick_beta_enabled || !kick_runtime_enabled {
                                let _ = evt_tx.send(AppEvent::Error {
                                    context: "Kick".into(),
                                    message: "Kick compatibility is disabled in Settings (beta).".into(),
                                }).await;
                                continue;
                            }
                            // Kick message sending is not yet supported (requires OAuth)
                            let _ = evt_tx.send(AppEvent::Error {
                                context: "Kick".into(),
                                message: "Sending messages to Kick channels is not yet supported.".into(),
                            }).await;
                        } else if channel.is_irc() {
                            if !irc_beta_enabled || !irc_runtime_enabled {
                                let _ = evt_tx.send(AppEvent::Error {
                                    context: "IRC".into(),
                                    message: "IRC compatibility is disabled in Settings (beta).".into(),
                                }).await;
                                continue;
                            }
                            let _ = irc_tx.send(GenericIrcSessionCommand::SendMessage(channel.clone(), text.clone())).await;
                        } else {
                            let _ = sess_tx.send(SessionCommand::SendMessage(channel.clone(), text.clone(), reply_to_msg_id)).await;
                        }

                        // Local echo: show the sent message immediately (Twitch only).
                        if let (Some(uname), Some(uid)) = (&auth_username, &auth_user_id) {
                        if channel.is_twitch() {
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
                                    paint: None,
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
                                reply: reply.clone(),
                                msg_kind: MsgKind::Chat,
                            };

                            if let Some(style) = stv_user_styles_resolved.get(uid) {
                                apply_7tv_cosmetics_to_sender(&mut echo.sender, style);
                            }

                            // Tokenize the echo message
                            {
                                let emote_guard = emote_index.read().unwrap();
                                echo.spans = crust_core::format::tokenize(
                                    &echo.raw_text,
                                    false,
                                    &echo.twitch_emotes,
                                    &|code| {
                                        resolve_emote(&emote_guard, code).map(|info| {
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
                            }

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
                                channel: channel.clone(),
                                message: echo,
                            }).await;
                        }
                        }

                        // Local echo for generic IRC channels (servers may not echo PRIVMSG).
                        // Handles plain text → echo to current channel, and
                        // /msg or /privmsg #chan text → echo body to target channel.
                        // Also works from the server tab for /msg and /privmsg.
                        if channel.is_irc() {
                            let irc_echo = if channel.is_irc_server_tab() {
                                // From the server tab, only echo /msg or /privmsg targeting a channel.
                                extract_irc_msg_echo(&text, &channel)
                            } else if !text.trim_start().starts_with('/')
                                && !is_raw_irc_protocol_line(&text)
                            {
                                // Plain text: echo to current channel.
                                Some((channel.clone(), text.clone()))
                            } else {
                                // Check for /msg or /privmsg targeting a channel.
                                extract_irc_msg_echo(&text, &channel)
                            };
                            if let Some((echo_channel, echo_text)) = irc_echo {
                                local_msg_id += 1;
                                let irc_nick = settings.irc_nick.trim();
                                let display = if irc_nick.is_empty() { "you" } else { irc_nick };
                                let mut echo = ChatMessage {
                                    id: MessageId(local_msg_id),
                                    server_id: None,
                                    timestamp: Utc::now(),
                                    channel: echo_channel.clone(),
                                    sender: Sender {
                                        user_id: UserId(display.to_owned()),
                                        login: display.to_lowercase(),
                                        display_name: display.to_owned(),
                                        color: None,
                                        paint: None,
                                        badges: Vec::new(),
                                    },
                                    raw_text: echo_text,
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

                                {
                                    let emote_guard = emote_index.read().unwrap();
                                    echo.spans = crust_core::format::tokenize(
                                        &echo.raw_text,
                                        false,
                                        &echo.twitch_emotes,
                                        &|code| {
                                            resolve_emote(&emote_guard, code).map(|info| {
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
                                }

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

                                let _ = evt_tx
                                    .send(AppEvent::MessageReceived {
                                        channel: echo_channel,
                                        message: echo,
                                    })
                                    .await;
                            }
                        }
                    }
                    AppCommand::FetchUserProfile { login } => {
                        let etx = evt_tx.clone();
                        tokio::spawn(async move { fetch_twitch_user_profile(&login, etx).await; });
                    }
                    AppCommand::TimeoutUser { channel, login, user_id, seconds, reason } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let moderator_id   = auth_user_id.clone();
                        let token          = settings.oauth_token.clone();
                        let client_id      = helix_client_id.clone();
                        let evt_tx2        = evt_tx.clone();
                        let ch_name        = channel.clone();
                        tokio::spawn(async move {
                            helix_ban_user(
                                &token, client_id.as_deref(),
                                broadcaster_id.as_deref(), moderator_id.as_deref(),
                                &user_id, Some(seconds),
                                reason.as_deref(),
                                &login, &ch_name, evt_tx2,
                            ).await;
                        });
                    }
                    AppCommand::BanUser { channel, login, user_id, reason } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let moderator_id   = auth_user_id.clone();
                        let token          = settings.oauth_token.clone();
                        let client_id      = helix_client_id.clone();
                        let evt_tx2        = evt_tx.clone();
                        let ch_name        = channel.clone();
                        tokio::spawn(async move {
                            helix_ban_user(
                                &token, client_id.as_deref(),
                                broadcaster_id.as_deref(), moderator_id.as_deref(),
                                &user_id, None,
                                reason.as_deref(),
                                &login, &ch_name, evt_tx2,
                            ).await;
                        });
                    }
                    AppCommand::UnbanUser { channel, login, user_id } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let moderator_id   = auth_user_id.clone();
                        let token          = settings.oauth_token.clone();
                        let client_id      = helix_client_id.clone();
                        let evt_tx2        = evt_tx.clone();
                        let ch_name        = channel.clone();
                        tokio::spawn(async move {
                            helix_unban_user(
                                &token, client_id.as_deref(),
                                broadcaster_id.as_deref(), moderator_id.as_deref(),
                                &user_id, &login, &ch_name, evt_tx2,
                            ).await;
                        });
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
                    AppCommand::ShowUserCard { login, channel } => {
                        let etx = evt_tx.clone();
                        tokio::spawn(async move {
                            fetch_user_profile_for_channel(&login, &channel, etx).await;
                        });
                    }
                }
            }

            // Background token validation results
            Some(val) = token_val_rx.recv() => {
                match val {
                    TokenValidationResult::Startup { token, result } => {
                        match result {
                            Ok(info) => {
                                let login = info.login;
                                info!("Saved token valid for user: {login}");
                                if !info.client_id.is_empty() {
                                    helix_client_id = Some(info.client_id);
                                }
                                if settings.username != login {
                                    settings.username = login.clone();
                                    if let Some(store) = &settings_store {
                                        let _ = store.save(&settings);
                                    }
                                }
                                // auth_in_progress was already set to true before spawn
                                let _ = sess_tx
                                    .send(SessionCommand::Authenticate { token, nick: login })
                                    .await;
                            }
                            Err(ValidateError::Unauthorized) => {
                                warn!("Saved token rejected by Twitch, clearing and starting anonymous");
                                auth_in_progress = false;
                                if let Some(store) = &settings_store {
                                    let _ = store.delete_token();
                                }
                                let _ = evt_tx.send(AppEvent::LoggedOut).await;
                            }
                            Err(ValidateError::Transient(e)) => {
                                warn!("Token validation failed ({e}), keeping token and starting anonymous");
                                auth_in_progress = false;
                                let _ = evt_tx.send(AppEvent::LoggedOut).await;
                            }
                        }
                    }
                    TokenValidationResult::Login { token, result } => {
                        match result {
                            Ok(info) => {
                                let login = info.login;
                                info!("Token valid for user: {login}");
                                if !info.client_id.is_empty() {
                                    helix_client_id = Some(info.client_id);
                                }
                                settings.username = login.clone();
                                settings.oauth_token = token.clone();
                                if let Some(acc) = settings.accounts.iter_mut().find(|a| a.username == login) {
                                    acc.oauth_token = token.clone();
                                } else {
                                    settings.accounts.push(crust_storage::AccountEntry {
                                        username: login.clone(),
                                        oauth_token: token.clone(),
                                    });
                                }
                                if let Some(store) = &settings_store {
                                    if let Err(e) = store.save(&settings) {
                                        warn!("Failed to save settings: {e}");
                                    }
                                    store.try_save_account_keyring(&login, &token);
                                }
                                if auth_username.is_some() {
                                    auth_username = None;
                                    auth_user_id = None;
                                    let _ = evt_tx.send(AppEvent::LoggedOut).await;
                                }
                                auth_in_progress = true;
                                let _ = sess_tx.send(SessionCommand::Authenticate {
                                    token,
                                    nick: login.clone(),
                                }).await;
                                let account_names: Vec<String> = settings.accounts.iter().map(|a| a.username.clone()).collect();
                                let default = if settings.default_account.is_empty() { None } else { Some(settings.default_account.clone()) };
                                let _ = evt_tx.send(AppEvent::AccountListUpdated {
                                    accounts: account_names,
                                    active: Some(login),
                                    default,
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
                    TokenValidationResult::AddAccount { token, result } => {
                        match result {
                            Ok(info) => {
                                let login = info.login;
                                info!("AddAccount: token valid for {login}");
                                if !info.client_id.is_empty() {
                                    helix_client_id = Some(info.client_id);
                                }
                                if let Some(acc) = settings.accounts.iter_mut().find(|a| a.username == login) {
                                    acc.oauth_token = token.clone();
                                } else {
                                    settings.accounts.push(crust_storage::AccountEntry {
                                        username: login.clone(),
                                        oauth_token: token.clone(),
                                    });
                                }
                                settings.username = login.clone();
                                settings.oauth_token = token.clone();
                                if let Some(store) = &settings_store {
                                    if let Err(e) = store.save(&settings) {
                                        warn!("Failed to save settings for AddAccount {login}: {e}");
                                    }
                                    store.try_save_account_keyring(&login, &token);
                                }
                                if auth_username.is_some() {
                                    auth_username = None;
                                    auth_user_id = None;
                                    let _ = evt_tx.send(AppEvent::LoggedOut).await;
                                }
                                auth_in_progress = true;
                                let _ = sess_tx.send(SessionCommand::Authenticate {
                                    token,
                                    nick: login.clone(),
                                }).await;
                                let account_names: Vec<String> = settings.accounts.iter().map(|a| a.username.clone()).collect();
                                let default = if settings.default_account.is_empty() { None } else { Some(settings.default_account.clone()) };
                                let _ = evt_tx.send(AppEvent::AccountListUpdated {
                                    accounts: account_names,
                                    active: Some(login),
                                    default,
                                }).await;
                            }
                            Err(e) => {
                                warn!("AddAccount token validation failed: {e}");
                                let _ = evt_tx.send(AppEvent::Error {
                                    context: "AddAccount".into(),
                                    message: format!("Invalid token: {e}"),
                                }).await;
                            }
                        }
                    }
                }
            }

            else => break,
        }
    }

    info!("Reducer loop exiting");
}

/// Parse Kick inline emote tags like `[emote:<id>:<code>]` and return:
/// - normalized message text with tags replaced by plain emote codes
/// - discovered `(id, code)` pairs for runtime emote registration
fn normalize_kick_inline_emotes(raw: &str) -> (String, Vec<(String, String)>) {
    if raw.is_empty() || !raw.contains("[emote:") {
        return (raw.to_owned(), Vec::new());
    }

    let mut out = String::with_capacity(raw.len());
    let mut found: Vec<(String, String)> = Vec::new();
    let mut i = 0usize;

    while i < raw.len() {
        let rest = &raw[i..];

        if let Some(tag_rest) = rest.strip_prefix("[emote:") {
            if let Some(id_sep_rel) = tag_rest.find(':') {
                let id_start_rel = "[emote:".len();
                let id = tag_rest[..id_sep_rel].trim();
                let code_start_rel = id_start_rel + id_sep_rel + 1;
                if code_start_rel <= rest.len() {
                    let code_and_tail = &rest[code_start_rel..];
                    if let Some(code_end_rel) = code_and_tail.find(']') {
                        let code = code_and_tail[..code_end_rel].trim();
                        if !id.is_empty() && !code.is_empty() {
                            let prev_is_ws = out
                                .chars()
                                .last()
                                .map(|c| c.is_whitespace())
                                .unwrap_or(true);
                            if !prev_is_ws {
                                out.push(' ');
                            }
                            out.push_str(code);
                            found.push((id.to_owned(), code.to_owned()));

                            i += code_start_rel + code_end_rel + 1; // include trailing ']'

                            if i < raw.len() {
                                let next = raw[i..].chars().next();
                                let next_needs_space = next
                                    .map(|c| {
                                        !c.is_whitespace()
                                            && !matches!(
                                                c,
                                                '.' | ',' | '!' | '?' | ':' | ';' | ')' | ']' | '}'
                                            )
                                    })
                                    .unwrap_or(false);
                                if next_needs_space {
                                    out.push(' ');
                                }
                            }
                            continue;
                        }
                    }
                }
            }
        }

        if let Some(ch) = rest.chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }

    (out, found)
}

fn kick_inline_emote_url(id: &str) -> String {
    format!("https://files.kick.com/emotes/{id}/fullsize")
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
    info!(
        "Loaded {total} global emotes (BTTV={}, FFZ={}, 7TV={})",
        b.len(),
        f.len(),
        s.len()
    );

    // Collect URLs of the newly-loaded emotes for prefetching.
    let new_urls: Vec<String> = f
        .iter()
        .chain(b.iter())
        .chain(s.iter())
        .map(|e| e.url_1x.clone())
        .collect();

    // Insert each provider under its own compound key so duplicates across
    // providers are preserved in the catalog.
    {
        let mut idx = index.write().unwrap();
        for e in f {
            idx.insert(emote_key(&e.provider, &e.code), e);
        }
        for e in b {
            idx.insert(emote_key(&e.provider, &e.code), e);
        }
        for e in s {
            idx.insert(emote_key(&e.provider, &e.code), e);
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
        for info in idx.values() {
            gc.insert(info.code.clone());
        }
    }

    // Send catalog snapshot to the UI
    send_emote_catalog(index, evt_tx, global_codes).await;

    // Eagerly prefetch only the newly-loaded emote images
    prefetch_emote_images(new_urls, cache, evt_tx);
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
    info!(
        "Loaded {} personal 7TV emotes for user-id {user_id}",
        emotes.len()
    );
    let new_urls: Vec<String> = emotes.iter().map(|e| e.url_1x.clone()).collect();
    {
        let mut idx = index.write().unwrap();
        for e in emotes {
            idx.insert(emote_key(&e.provider, &e.code), e);
        }
    }
    send_emote_catalog(index, evt_tx, global_codes).await;
    prefetch_emote_images(new_urls, cache, evt_tx);
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
        let _ = evt_tx
            .send(AppEvent::ChannelEmotesLoaded {
                channel: ChannelId::new(channel_name),
                count: 0,
            })
            .await;
        return;
    }
    info!(
        "Loaded {total} channel emotes for #{channel_name} (BTTV={}, FFZ={}, 7TV={})",
        b.len(),
        f.len(),
        s.len()
    );

    // Collect URLs of the newly-loaded emotes for prefetching.
    let new_urls: Vec<String> = f
        .iter()
        .chain(b.iter())
        .chain(s.iter())
        .map(|e| e.url_1x.clone())
        .collect();

    {
        let mut idx = index.write().unwrap();
        for e in f {
            idx.insert(emote_key(&e.provider, &e.code), e);
        }
        for e in b {
            idx.insert(emote_key(&e.provider, &e.code), e);
        }
        for e in s {
            idx.insert(emote_key(&e.provider, &e.code), e);
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

    let _ = evt_tx
        .send(AppEvent::ChannelEmotesLoaded {
            channel: ChannelId::new(channel_name),
            count: total,
        })
        .await;

    // Eagerly prefetch only the newly-loaded channel emote images
    prefetch_emote_images(new_urls, cache, evt_tx);
}

/// Load Kick channel emotes from Kick-native and 7TV(Kick) providers.
async fn load_kick_channel_emotes(
    channel: &ChannelId,
    kick_user_id: u64,
    index: &EmoteIndex,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
    global_codes: &GlobalCodes,
) {
    let slug = channel.display_name().to_owned();
    info!(
        "Loading Kick emotes for {} (kick user-id {kick_user_id})",
        channel.display_name()
    );

    let kick = KickProvider::new();
    let stv = SevenTvProvider::new();

    let (kick_emotes, stv_emotes) = tokio::join!(kick.load_channel(&slug), async {
        if kick_user_id > 0 {
            stv.load_kick_channel(&kick_user_id.to_string()).await
        } else {
            vec![]
        }
    },);

    let total = kick_emotes.len() + stv_emotes.len();
    if total == 0 {
        warn!("No Kick emotes found for {}", channel.display_name());
        let _ = evt_tx
            .send(AppEvent::ChannelEmotesLoaded {
                channel: channel.clone(),
                count: 0,
            })
            .await;
        return;
    }

    info!(
        "Loaded {total} Kick channel emotes for {} (Kick={}, 7TV={})",
        channel.display_name(),
        kick_emotes.len(),
        stv_emotes.len(),
    );

    let new_urls: Vec<String> = kick_emotes
        .iter()
        .chain(stv_emotes.iter())
        .map(|e| e.url_1x.clone())
        .collect();

    {
        let mut idx = index.write().unwrap();
        for e in kick_emotes {
            idx.insert(emote_key(&e.provider, &e.code), e);
        }
        for e in stv_emotes {
            idx.insert(emote_key(&e.provider, &e.code), e);
        }
    }

    if let Some(cache) = cache {
        let idx = index.read().unwrap();
        let emotes: Vec<EmoteInfo> = idx.values().cloned().collect();
        drop(idx);
        cache.register(emotes);
    }

    send_emote_catalog(index, evt_tx, global_codes).await;

    let _ = evt_tx
        .send(AppEvent::ChannelEmotesLoaded {
            channel: channel.clone(),
            count: total,
        })
        .await;

    prefetch_emote_images(new_urls, cache, evt_tx);
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
    let _ = evt_tx
        .send(AppEvent::EmoteCatalogUpdated { emotes: entries })
        .await;
}

/// Eagerly prefetch emote images in the background so they're available
/// in the emote picker and `:` autocomplete without waiting for lazy fetch.
fn prefetch_emote_images(
    urls: Vec<String>,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    if urls.is_empty() {
        return;
    }
    info!("Prefetching {} emote images…", urls.len());
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

// 7TV cosmetics

fn seven_tv_color_to_rgba(color: i32) -> (u8, u8, u8, u8) {
    let raw = color as u32;
    let r = ((raw >> 24) & 0xFF) as u8;
    let g = ((raw >> 16) & 0xFF) as u8;
    let b = ((raw >> 8) & 0xFF) as u8;
    let a = (raw & 0xFF) as u8;
    (r, g, b, a)
}

fn seven_tv_color_to_hex(color: i32) -> Option<String> {
    if color == 0 {
        return None;
    }
    let (r, g, b, a) = seven_tv_color_to_rgba(color);
    if a == 0 {
        return None;
    }
    Some(format!("#{r:02X}{g:02X}{b:02X}"))
}

fn seven_tv_badge_url(host_url: &str, file_name: &str) -> String {
    let base = if host_url.starts_with("//") {
        format!("https:{host_url}")
    } else {
        host_url.to_owned()
    };
    format!("{}/{}", base.trim_end_matches('/'), file_name)
}

fn choose_7tv_badge_file(files: &[SevenTvBadgeFile]) -> Option<&SevenTvBadgeFile> {
    files
        .iter()
        .find(|f| f.name.starts_with("2x."))
        .or_else(|| files.iter().find(|f| f.name.starts_with("1x.")))
        .or_else(|| files.first())
}

fn fallback_hex_from_paint(paint: &SenderPaint) -> Option<String> {
    paint
        .stops
        .first()
        .and_then(|s| seven_tv_color_to_hex(s.color))
}

fn resolve_7tv_user_style(
    style: &SevenTvUserStyleRaw,
    paints: &HashMap<String, SenderPaint>,
    badges: &HashMap<String, SevenTvBadgeMeta>,
) -> SevenTvResolvedStyle {
    let paint = style
        .paint_id
        .as_ref()
        .and_then(|id| paints.get(id))
        .cloned();

    let color_hex = style
        .color
        .and_then(seven_tv_color_to_hex)
        .or_else(|| paint.as_ref().and_then(fallback_hex_from_paint));

    let badge = style.badge_id.as_ref().and_then(|id| badges.get(id)).map(|b| {
        Badge {
            name: "7tv".to_owned(),
            version: b.tooltip.clone().unwrap_or_else(|| "1".to_owned()),
            url: Some(b.url.clone()),
        }
    });

    SevenTvResolvedStyle {
        color_hex,
        paint,
        badge,
    }
}

fn apply_7tv_cosmetics_to_sender(sender: &mut Sender, style: &SevenTvResolvedStyle) {
    if let Some(ref color) = style.color_hex {
        sender.color = Some(color.clone());
    }

    sender.paint = style.paint.clone();

    if let Some(ref badge) = style.badge {
        let already_has = sender.badges.iter().any(|b| {
            b.url.as_deref() == badge.url.as_deref() || b.name.eq_ignore_ascii_case("7tv")
        });
        if !already_has {
            sender.badges.insert(0, badge.clone());
        }
    }
}

async fn load_7tv_cosmetics_catalog(client: &reqwest::Client) -> Option<(HashMap<String, SenderPaint>, HashMap<String, SevenTvBadgeMeta>)> {
    #[derive(serde::Deserialize)]
    struct RespData {
        cosmetics: Cosmetics,
    }

    #[derive(serde::Deserialize)]
    struct Cosmetics {
        paints: Vec<PaintNode>,
        badges: Vec<BadgeNode>,
    }

    #[derive(serde::Deserialize)]
    struct PaintNode {
        id: String,
        name: String,
        #[serde(rename = "function")]
        function_name: String,
        #[serde(default)]
        angle: f32,
        #[serde(default)]
        repeat: bool,
        #[serde(default)]
        image_url: String,
        #[serde(default)]
        stops: Vec<PaintStopNode>,
    }

    #[derive(serde::Deserialize)]
    struct PaintStopNode {
        at: f32,
        color: i32,
    }

    #[derive(serde::Deserialize)]
    struct BadgeNode {
        id: String,
        tooltip: Option<String>,
        host: BadgeHost,
    }

    #[derive(serde::Deserialize)]
    struct BadgeHost {
        url: String,
        files: Vec<SevenTvBadgeFile>,
    }

    let query = r#"
        query {
            cosmetics {
                paints {
                    id
                    name
                    function
                    angle
                    repeat
                    image_url
                    stops {
                        at
                        color
                    }
                }
                badges {
                    id
                    tooltip
                    host {
                        url
                        files {
                            name
                        }
                    }
                }
            }
        }
    "#;

    let resp = match client
        .post(SEVENTV_GQL_URL)
        .json(&serde_json::json!({ "query": query }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("7TV cosmetics fetch failed: {e}");
            return None;
        }
    };

    let payload = match resp.json::<SevenTvGraphQlResponse<RespData>>().await {
        Ok(p) => p,
        Err(e) => {
            warn!("7TV cosmetics parse failed: {e}");
            return None;
        }
    };

    if !payload.errors.is_empty() {
        let messages = payload
            .errors
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        warn!("7TV cosmetics GraphQL errors: {messages}");
    }

    let Some(data) = payload.data else {
        return None;
    };

    let paints: HashMap<String, SenderPaint> = data
        .cosmetics
        .paints
        .into_iter()
        .map(|p| {
            let paint = SenderPaint {
                id: p.id.clone(),
                name: p.name,
                function: p.function_name,
                angle: p.angle,
                repeat: p.repeat,
                image_url: if p.image_url.is_empty() {
                    None
                } else {
                    Some(p.image_url)
                },
                stops: p
                    .stops
                    .into_iter()
                    .map(|s| SenderPaintStop {
                        at: s.at,
                        color: s.color,
                    })
                    .collect(),
            };
            (p.id, paint)
        })
        .collect();

    let badges: HashMap<String, SevenTvBadgeMeta> = data
        .cosmetics
        .badges
        .into_iter()
        .filter_map(|b| {
            let file = choose_7tv_badge_file(&b.host.files)?;
            Some((
                b.id,
                SevenTvBadgeMeta {
                    tooltip: b.tooltip,
                    url: seven_tv_badge_url(&b.host.url, &file.name),
                },
            ))
        })
        .collect();

    info!(
        "Loaded 7TV cosmetics catalog (paints={}, badges={})",
        paints.len(),
        badges.len()
    );

    Some((paints, badges))
}

async fn load_7tv_user_style_for_twitch(client: &reqwest::Client, twitch_user_id: &str) -> Option<SevenTvUserStyleRaw> {
    #[derive(serde::Deserialize)]
    struct RespData {
        #[serde(rename = "userByConnection")]
        user_by_connection: Option<UserNode>,
    }

    #[derive(serde::Deserialize)]
    struct UserNode {
        style: StyleNode,
    }

    #[derive(serde::Deserialize)]
    struct StyleNode {
        color: i32,
        paint_id: Option<String>,
        badge_id: Option<String>,
    }

    if twitch_user_id.trim().is_empty() {
        return None;
    }

    let query = r#"
        query($id: String!) {
            userByConnection(platform: TWITCH, id: $id) {
                style {
                    color
                    paint_id
                    badge_id
                }
            }
        }
    "#;

    let resp = match client
        .post(SEVENTV_GQL_URL)
        .json(&serde_json::json!({
            "query": query,
            "variables": { "id": twitch_user_id }
        }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            debug!("7TV user style fetch failed for {twitch_user_id}: {e}");
            return None;
        }
    };

    let payload = match resp.json::<SevenTvGraphQlResponse<RespData>>().await {
        Ok(p) => p,
        Err(e) => {
            debug!("7TV user style parse failed for {twitch_user_id}: {e}");
            return None;
        }
    };

    if !payload.errors.is_empty() {
        let messages = payload
            .errors
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        debug!("7TV user style GraphQL errors for {twitch_user_id}: {messages}");
    }

    let style = payload
        .data
        .and_then(|d| d.user_by_connection)
        .map(|u| u.style)
        .unwrap_or(StyleNode {
            color: 0,
            paint_id: None,
            badge_id: None,
        });

    Some(SevenTvUserStyleRaw {
        color: if style.color == 0 {
            None
        } else {
            Some(style.color)
        },
        paint_id: style.paint_id.filter(|s| !s.is_empty()),
        badge_id: style.badge_id.filter(|s| !s.is_empty()),
    })
}

#[derive(Debug, serde::Deserialize)]
struct SevenTvBadgeFile {
    name: String,
}

// Badge loading

/// Resolve a badge image URL from the badge map.
///
/// Twitch IRC sends some badge versions as cumulative counts (e.g.
/// `subscriber/28`, `bits/5000`) that don't directly match the fixed tier
/// version keys stored by the badge API (e.g. `"0"`, `"3"`, `"6"`, `"24"`).
/// When an exact match is not found this function falls back to the highest
/// available version that is numerically ≤ the requested version, which
/// selects the correct tier image without requiring a perfect key match.
fn resolve_badge_url(
    map: &std::collections::HashMap<(String, String), String>,
    name: &str,
    version: &str,
) -> Option<String> {
    // Fast path: exact match.
    if let Some(url) = map.get(&(name.to_owned(), version.to_owned())) {
        return Some(url.clone());
    }
    // Slow path: numeric fallback - find the highest available version ≤ version.
    let target: u64 = version.parse().ok()?;
    let mut best: Option<(u64, &String)> = None;
    for ((n, v), url) in map {
        if n == name {
            if let Ok(candidate) = v.parse::<u64>() {
                if candidate <= target && best.map_or(true, |(b, _)| candidate > b) {
                    best = Some((candidate, url));
                }
            }
        }
    }
    best.map(|(_, url)| url.clone())
}

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
                    let new: Vec<String> = map
                        .values()
                        .filter(|u| !before.contains(*u))
                        .cloned()
                        .collect();
                    info!(
                        "Loaded {} global badges via IVR",
                        after_count - before.len()
                    );
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
                    let new: Vec<String> = map
                        .values()
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
        Ok(resp) => warn!(
            "IVR channel badges returned HTTP {} for room {room_id}",
            resp.status()
        ),
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
    if urls.is_empty() {
        return;
    }
    info!("Prefetching {} badge images…", urls.len());
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

/// Fetch a single emote/emoji/badge image and send raw bytes to UI.
async fn fetch_emote_image(url: &str, cache: &Option<EmoteCache>, evt_tx: &mpsc::Sender<AppEvent>) {
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
            // Emit a zero-byte stub so the loading screen can count this
            // fetch as settled (prevents hanging on failures).
            let _ = evt_tx
                .send(AppEvent::EmoteImageReady {
                    uri: url.to_owned(),
                    width: 0,
                    height: 0,
                    raw_bytes: vec![],
                })
                .await;
        }
    }
}

async fn fetch_and_decode_raw(url: &str) -> Result<(u32, u32, Vec<u8>), crust_emotes::EmoteError> {
    let client = reqwest::Client::new();
    let resp = client.get(url).send().await?;
    let raw = resp.bytes().await?;
    let raw_vec = raw.to_vec();
    // Read dimensions from header only - no full RGBA decode needed
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
    stv_update_tx: &mpsc::Sender<SevenTvCosmeticUpdate>,
) {
    let ch = channel.trim_start_matches('#');
    let channel_id = crust_core::model::ChannelId::new(ch);

    // NOTE: the correct path is /recent-messages/ (hyphen), not /recent_messages/.
    let robotty_url =
        format!("https://recent-messages.robotty.de/api/v2/recent-messages/{ch}?limit=800");
    let ivr_url = format!("https://logs.ivr.fi/channel/{ch}?json=1&reverse=true&limit=800");

    let client = reqwest::Client::new();

    // Try robotty first; it covers all channels (including small ones).
    // Fall back to IVR if robotty fails or returns nothing.
    let raw_lines: Vec<String> = 'fetch: {
        if let Ok(resp) = client
            .get(&robotty_url)
            .header("Accept", "application/json")
            .send()
            .await
        {
            if resp.status().is_success() {
                if let Ok(text) = resp.text().await {
                    #[derive(serde::Deserialize)]
                    struct RobottyResponse {
                        messages: Vec<String>,
                    }
                    if let Ok(p) = serde_json::from_str::<RobottyResponse>(&text) {
                        if !p.messages.is_empty() {
                            break 'fetch p.messages;
                        }
                    }
                }
            }
        }
        // IVR fallback
        match client
            .get(&ivr_url)
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(text) = resp.text().await {
                    #[derive(serde::Deserialize)]
                    struct IvrMsg {
                        raw: String,
                    }
                    #[derive(serde::Deserialize)]
                    struct IvrResp {
                        messages: Vec<IvrMsg>,
                    }
                    if let Ok(mut p) = serde_json::from_str::<IvrResp>(&text) {
                        p.messages.reverse(); // IVR is newest-first
                        break 'fetch p.messages.into_iter().map(|m| m.raw).collect();
                    }
                }
                Vec::new()
            }
            Ok(resp) => {
                warn!(
                    "chat-history: both sources failed for #{ch} (IVR HTTP {})",
                    resp.status()
                );
                Vec::new()
            }
            Err(e) => {
                warn!("chat-history: both sources failed for #{ch}: {e}");
                Vec::new()
            }
        }
    };

    if raw_lines.is_empty() {
        info!("Loaded 0 historical messages for #{ch}");
        let _ = evt_tx
            .send(AppEvent::HistoryLoaded {
                channel: channel_id,
                messages: Vec::new(),
            })
            .await;
        return;
    }

    // ── Snapshot shared state once before the parse loop ────────────────────
    // Taking these locks inside the loop (800+ times) is expensive; snapshot
    // once and release immediately so other tasks aren't blocked.
    let emote_snapshot: HashMap<String, EmoteInfo> = {
        let guard = emote_index.read().unwrap();
        guard.clone()
    };
    let badge_snapshot: HashMap<(String, String), String> = {
        let bm = badge_map.read().unwrap();
        bm.clone()
    };
    let local_nick_owned = local_nick.map(str::to_owned);

    // ── Move the CPU-bound parse + tokenize loop off the async executor ──────
    // Tokenization and IRC parsing are synchronous CPU work.  Running them
    // directly on the tokio thread pool starves other async tasks; moving to
    // spawn_blocking gives those threads back.
    let (messages, image_urls) = tokio::task::spawn_blocking(move || {
        let mut messages: Vec<ChatMessage> = Vec::with_capacity(raw_lines.len());
        // Deduplicate image fetch URLs across all history messages.
        let mut seen_urls: HashSet<String> = HashSet::new();
        let mut image_urls: Vec<String> = Vec::new();

        for line in &raw_lines {
            let irc_msg = match parse_line(line) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if irc_msg.command != "PRIVMSG" {
                continue;
            }

            let id = HISTORY_MSG_ID.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            let mut msg = match parse_privmsg_irc(&irc_msg, local_nick_owned.as_deref(), id) {
                Some(m) => m,
                None => continue,
            };

            // Tokenize spans
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

            // Resolve badge URLs from the snapshot (no lock needed)
            for badge in &mut msg.sender.badges {
                badge.url = resolve_badge_url(&badge_snapshot, &badge.name, &badge.version);
            }

            // Mention detection
            if let Some(ref nick) = local_nick_owned {
                let nick_lower = nick.to_lowercase();
                let has_mention = msg
                    .raw_text
                    .to_lowercase()
                    .contains(&format!("@{nick_lower}"));
                let is_reply_to_me = msg
                    .reply
                    .as_ref()
                    .map(|r| r.parent_user_login.to_lowercase() == nick_lower)
                    .unwrap_or(false);
                msg.flags.is_mention = has_mention || is_reply_to_me;
            }

            // Collect unique image URLs (emotes, emoji, badges) for batch prefetch
            for span in &msg.spans {
                let url = match span {
                    crust_core::Span::Emote { url, .. } => Some(url.clone()),
                    crust_core::Span::Emoji { url, .. } => Some(url.clone()),
                    _ => None,
                };
                if let Some(u) = url {
                    if seen_urls.insert(u.clone()) {
                        image_urls.push(u);
                    }
                }
            }
            for badge in &msg.sender.badges {
                if let Some(ref u) = badge.url {
                    if seen_urls.insert(u.clone()) {
                        image_urls.push(u.clone());
                    }
                }
            }

            messages.push(msg);
        }
        (messages, image_urls)
    })
    .await
    .unwrap_or_default();

    if messages.is_empty() {
        info!("Loaded 0 historical messages for #{ch}");
        let _ = evt_tx
            .send(AppEvent::HistoryLoaded {
                channel: channel_id,
                messages,
            })
            .await;
        return;
    }

    info!("Loaded {} historical messages for #{ch}", messages.len());

    // Collect unique Twitch user-ids from history so 7TV can resolve
    // their cosmetics (paints/badges) retroactively.
    {
        let mut history_user_ids: Vec<String> = messages
            .iter()
            .map(|m| m.sender.user_id.0.trim().to_owned())
            .filter(|id| !id.is_empty())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        history_user_ids.sort();
        if !history_user_ids.is_empty() {
            let _ = stv_update_tx
                .send(SevenTvCosmeticUpdate::BatchUserLookup { user_ids: history_user_ids })
                .await;
        }
    }

    // Batch-prefetch all unique image URLs with a semaphore (same path as
    // emote/badge prefetch, which also emits ImagePrefetchQueued for the
    // loading screen counter).
    if !image_urls.is_empty() {
        prefetch_emote_images(image_urls, emote_cache, evt_tx);
    }

    let _ = evt_tx
        .send(AppEvent::HistoryLoaded {
            channel: channel_id,
            messages,
        })
        .await;
}

// Token validation

/// Call `POST /helix/moderation/bans` to timeout or permanently ban a user.
///
/// `duration_secs` = `None` → permanent ban; `Some(n)` → timeout for `n` seconds.
/// On failure, injects a local error message into the channel so the user can see what went wrong.
async fn helix_ban_user(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    moderator_id: Option<&str>,
    target_user_id: &str,
    duration_secs: Option<u32>,
    reason: Option<&str>,
    target_login: &str,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let (Some(cid), Some(bid), Some(mid)) = (client_id, broadcaster_id, moderator_id) else {
        warn!(
            "helix_ban_user: missing credentials (cid={:?} bid={:?} mid={:?})",
            client_id, broadcaster_id, moderator_id
        );
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Moderation".into(),
                message: "Cannot moderate: missing Twitch credentials. Reconnect and try again."
                    .into(),
            })
            .await;
        return;
    };

    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    let url = format!(
        "https://api.twitch.tv/helix/moderation/bans?broadcaster_id={bid}&moderator_id={mid}"
    );

    #[derive(serde::Serialize)]
    struct BanData<'a> {
        user_id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<&'a str>,
    }
    #[derive(serde::Serialize)]
    struct BanBody<'a> {
        data: BanData<'a>,
    }

    let body = BanBody {
        data: BanData {
            user_id: target_user_id,
            duration: duration_secs,
            reason,
        },
    };

    let client = reqwest::Client::new();
    let resp = match client
        .post(&url)
        .header("Authorization", format!("Bearer {bare}"))
        .header("Client-Id", cid)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("helix_ban_user: request failed: {e}");
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Moderation".into(),
                    message: format!("Moderation request failed: {e}"),
                })
                .await;
            return;
        }
    };

    let status = resp.status();
    if status.is_success() {
        let verb = if duration_secs.is_some() {
            let secs = duration_secs.unwrap();
            let (n, unit) = if secs < 60 {
                (secs, "s")
            } else if secs < 3600 {
                (secs / 60, "m")
            } else if secs < 86400 {
                (secs / 3600, "h")
            } else {
                (secs / 86400, "d")
            };
            format!("timed out for {n}{unit}")
        } else {
            "permanently banned".into()
        };
        info!("Moderation: {target_login} {verb} in #{channel}");
    } else {
        let body_text = resp.text().await.unwrap_or_default();
        warn!("helix_ban_user: HTTP {status} - {body_text}");
        // Extract a human-readable message from the Helix error response.
        let helix_msg = serde_json::from_str::<serde_json::Value>(&body_text)
            .ok()
            .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_owned))
            .unwrap_or_else(|| format!("HTTP {status}"));
        let action = if duration_secs.is_some() {
            "timeout"
        } else {
            "ban"
        };
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Moderation".into(),
                message: format!("Could not {action} {target_login}: {helix_msg}"),
            })
            .await;
    }
}

/// Call `DELETE /helix/moderation/bans` to lift a ban or active timeout.
async fn helix_unban_user(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    moderator_id: Option<&str>,
    target_user_id: &str,
    target_login: &str,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let (Some(cid), Some(bid), Some(mid)) = (client_id, broadcaster_id, moderator_id) else {
        warn!("helix_unban_user: missing credentials");
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Moderation".into(),
                message: "Cannot unban: missing Twitch credentials. Reconnect and try again."
                    .into(),
            })
            .await;
        return;
    };

    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    let url = format!(
        "https://api.twitch.tv/helix/moderation/bans\
         ?broadcaster_id={bid}&moderator_id={mid}&user_id={target_user_id}"
    );

    let client = reqwest::Client::new();
    let resp = match client
        .delete(&url)
        .header("Authorization", format!("Bearer {bare}"))
        .header("Client-Id", cid)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("helix_unban_user: request failed: {e}");
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Moderation".into(),
                    message: format!("Unban request failed: {e}"),
                })
                .await;
            return;
        }
    };

    let status = resp.status();
    if status.is_success() || status.as_u16() == 204 {
        info!("Moderation: {target_login} unbanned/untimedout in #{channel}");
    } else {
        let body_text = resp.text().await.unwrap_or_default();
        warn!("helix_unban_user: HTTP {status} - {body_text}");
        let helix_msg = serde_json::from_str::<serde_json::Value>(&body_text)
            .ok()
            .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_owned))
            .unwrap_or_else(|| format!("HTTP {status}"));
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Moderation".into(),
                message: format!("Could not unban {target_login}: {helix_msg}"),
            })
            .await;
    }
}

/// Error returned by [`validate_token`].
#[derive(Debug)]
enum ValidateError {
    /// The Twitch API explicitly rejected the token (HTTP 401 / 403).
    /// The token should be deleted from storage.
    Unauthorized,
    /// A transient problem (network error, server 5xx, parse failure, …).
    /// The token should be kept so it can be retried next launch.
    Transient(String),
}

impl std::fmt::Display for ValidateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => write!(f, "token rejected by Twitch (unauthorized)"),
            Self::Transient(e) => write!(f, "{e}"),
        }
    }
}

/// Information returned by a successful token validation.
struct ValidateInfo {
    /// Twitch login name (always lowercase).
    login: String,
    /// Twitch user-id string for the token owner (available for future use).
    #[allow(dead_code)]
    user_id: String,
    /// Client-id of the application the token was issued to.
    client_id: String,
}

/// Validate a Twitch OAuth token via the Twitch API and return the login name.
async fn validate_token(token: &str) -> Result<ValidateInfo, ValidateError> {
    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    let client = reqwest::Client::new();
    let resp = client
        .get("https://id.twitch.tv/oauth2/validate")
        .header("Authorization", format!("OAuth {bare}"))
        .send()
        .await
        .map_err(|e| ValidateError::Transient(format!("HTTP error: {e}")))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(ValidateError::Unauthorized);
    }
    if !status.is_success() {
        return Err(ValidateError::Transient(format!(
            "Token rejected (HTTP {status})"
        )));
    }

    #[derive(serde::Deserialize)]
    struct ValidateResponse {
        login: String,
        #[serde(default)]
        user_id: String,
        #[serde(default)]
        client_id: String,
    }

    let body = resp.json::<ValidateResponse>().await.map_err(|e| {
        ValidateError::Transient(format!("Failed to parse validation response: {e}"))
    })?;

    Ok(ValidateInfo {
        login: body.login,
        user_id: body.user_id,
        client_id: body.client_id,
    })
}

// User profile

/// Fetch a user profile appropriate for the channel platform.
async fn fetch_user_profile_for_channel(
    login: &str,
    channel: &ChannelId,
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
        fetch_twitch_user_profile(login, evt_tx).await;
    }
}

/// Fetch a Twitch user profile from the IVR API (no auth required) and send
/// `AppEvent::UserProfileLoaded`. Also pre-fetches avatar bytes so the popup
/// can show the real avatar immediately.
async fn fetch_twitch_user_profile(login: &str, evt_tx: mpsc::Sender<AppEvent>) {
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
        /// Non-null while the channel is live.
        stream: Option<IvrStream>,
        /// Info about the most recent broadcast.
        #[serde(rename = "lastBroadcast")]
        last_broadcast: Option<IvrBroadcast>,
        /// Non-null if the account is banned/suspended.
        #[serde(rename = "banStatus")]
        ban_status: Option<IvrBanStatus>,
    }

    let url = format!("https://api.ivr.fi/v2/twitch/user?login={login}");
    let client = reqwest::Client::new();
    let resp = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            warn!("IVR user fetch returned HTTP {} for {login}", r.status());
            let _ = evt_tx
                .send(AppEvent::UserProfileUnavailable {
                    login: login.to_owned(),
                })
                .await;
            return;
        }
        Err(e) => {
            warn!("IVR user fetch failed for {login}: {e}");
            let _ = evt_tx
                .send(AppEvent::UserProfileUnavailable {
                    login: login.to_owned(),
                })
                .await;
            return;
        }
    };

    let users: Vec<IvrUser> = match resp.json().await {
        Ok(u) => u,
        Err(e) => {
            warn!("IVR user response parse failed for {login}: {e}");
            let _ = evt_tx
                .send(AppEvent::UserProfileUnavailable {
                    login: login.to_owned(),
                })
                .await;
            return;
        }
    };

    let Some(user) = users.into_iter().next() else {
        warn!("IVR returned no user for {login}");
        let _ = evt_tx
            .send(AppEvent::UserProfileUnavailable {
                login: login.to_owned(),
            })
            .await;
        return;
    };

    let avatar_url = user.logo.clone();

    let is_live = user.stream.is_some();
    let stream_title = user
        .stream
        .as_ref()
        .map(|s| s.title.clone())
        .filter(|s| !s.is_empty());
    let stream_game = user
        .stream
        .as_ref()
        .and_then(|s| s.game.as_ref())
        .map(|g| g.display_name.clone())
        .filter(|s| !s.is_empty());
    let stream_viewers = user.stream.as_ref().map(|s| s.viewers_count);
    let stream_started = user.stream.as_ref().and_then(|s| s.started_at.clone());
    let last_broadcast_at =
        stream_started.or_else(|| user.last_broadcast.and_then(|b| b.started_at));
    let is_banned = user.roles.as_ref().map_or(false, |r| r.is_banned) || user.ban_status.is_some();
    let ban_reason = user.ban_status.and_then(|b| b.reason);

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
        chat_color: user.chat_color,
        is_live,
        stream_title,
        stream_game,
        stream_viewers,
        last_broadcast_at,
        is_banned,
        ban_reason,
    };

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

    let _ = evt_tx.send(AppEvent::UserProfileLoaded { profile }).await;
}

/// Fetch a Kick user profile via Kick's public channel API.
async fn fetch_kick_user_profile(login: &str, evt_tx: mpsc::Sender<AppEvent>) {
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
            "Mozilla/5.0 (X11; Linux x86_64) CrustChat/0.1",
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
async fn fetch_self_avatar(login: &str, evt_tx: mpsc::Sender<AppEvent>) {
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

// System-message helpers

/// Extract echo info from an IRC `/msg` or `/privmsg` command that targets a
/// channel (e.g. `/msg ##chat hello`).  Returns `(target_channel_id, body_text)`
/// so the caller can emit a local echo.  Returns `None` for non-channel targets
/// (e.g. NickServ) and non-msg commands.
fn extract_irc_msg_echo(text: &str, source_channel: &ChannelId) -> Option<(ChannelId, String)> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let cmd_line = trimmed.trim_start_matches('/').trim_start();
    let (cmd, rest) = cmd_line
        .split_once(char::is_whitespace)
        .map(|(c, r)| (c, r.trim_start()))
        .unwrap_or((cmd_line, ""));
    if !matches!(cmd.to_ascii_lowercase().as_str(), "msg" | "privmsg") {
        return None;
    }
    let mut parts = rest.splitn(2, char::is_whitespace);
    let target = parts.next()?.trim();
    let body = parts.next()?.trim_start();
    // Strip optional leading ':' (IRC protocol format).
    let body = body.strip_prefix(':').unwrap_or(body);
    // Only echo for channel targets (starting with #).
    if !target.starts_with('#') || body.is_empty() {
        return None;
    }
    let irc_target = source_channel.irc_target()?;
    // No gvbhrmalize: strip first '#' for internal ChannelId form (##chat → #chat).
    let ch_name = target
        .strip_prefix('#')
        .unwrap_or(target)
        .to_ascii_lowercase();
    let echo_ch = ChannelId::irc(
        &irc_target.host,
        irc_target.port,
        irc_target.tls,
        &ch_name,
    );
    Some((echo_ch, body.to_owned()))
}

/// Construct a system (non-chat) ChatMessage for inline display in a channel.
fn make_system_message(
    id: u64,
    channel: ChannelId,
    text: String,
    timestamp: chrono::DateTime<Utc>,
    kind: MsgKind,
) -> ChatMessage {
    use smallvec::smallvec;
    let spans = smallvec![crust_core::model::Span::Text {
        text: text.clone(),
        is_action: false
    }];
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
            paint: None,
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
        format!(
            "{login} was timed out for {}h {}m.",
            seconds / 3600,
            (seconds % 3600) / 60
        )
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
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", url])
            .spawn();
    }
}

// Link preview fetch

async fn fetch_link_preview(
    url: &str,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let send_empty = |url: &str| AppEvent::LinkPreviewReady {
        url: url.to_owned(),
        title: None,
        description: None,
        thumbnail_url: None,
    };

    let client = match reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; crust-chat/1.0; +https://github.com/crust)")
        .timeout(std::time::Duration::from_secs(6))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            let _ = evt_tx.send(send_empty(url)).await;
            return;
        }
    };

    let resp = match client.get(url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => {
            let _ = evt_tx.send(send_empty(url)).await;
            return;
        }
    };

    // Only parse HTML
    let ct = resp
        .headers()
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
        Err(_) => {
            let _ = evt_tx.send(send_empty(url)).await;
            return;
        }
    };
    // Only read the first 64 KB to avoid processing megabyte HTML files.
    let html = String::from_utf8_lossy(&bytes[..bytes.len().min(65_536)]);

    let title = og_meta(&html, "og:title")
        .or_else(|| og_meta(&html, "twitter:title"))
        .or_else(|| html_title(&html));
    let description =
        og_meta(&html, "og:description").or_else(|| og_meta(&html, "twitter:description"));
    let thumbnail_url = og_meta(&html, "og:image").or_else(|| og_meta(&html, "twitter:image"));

    // Kick off thumbnail image fetch so bytes land in emote_bytes.
    if let Some(ref img) = thumbnail_url {
        fetch_emote_image(img, cache, evt_tx).await;
    }

    let _ = evt_tx
        .send(AppEvent::LinkPreviewReady {
            url: url.to_owned(),
            title,
            description,
            thumbnail_url,
        })
        .await;
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

        let has_prop = tag_lower.contains(&format!("property=\"{prop_lower}\""))
            || tag_lower.contains(&format!("property='{prop_lower}'"))
            || tag_lower.contains(&format!("name=\"{prop_lower}\""))
            || tag_lower.contains(&format!("name='{prop_lower}'"));

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
    if text.is_empty() {
        None
    } else {
        Some(html_entities(&text))
    }
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

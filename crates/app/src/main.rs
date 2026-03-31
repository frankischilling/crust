use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use anyhow::Result;
use directories::ProjectDirs;
use eframe::egui;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use chrono::Utc;
use crust_core::events::{AppCommand, AppEvent, ConnectionState};
use crust_core::model::{
    Badge, ChannelId, ChatMessage, EmoteCatalogEntry, MessageFlags, MessageId, MsgKind, Sender,
    UserId,
};
use crust_emotes::{
    cache::EmoteCache,
    providers::{
        BttvProvider, EmoteInfo, FfzProvider, KickProvider, SevenTvProvider, TwitchGlobalProvider,
    },
    EmoteProvider,
};
use crust_kick::session::{KickEvent, KickSession, KickSessionCommand};
use crust_storage::{AppSettings, LogStore, SettingsStore};
use crust_twitch::session::generic_irc::{
    is_raw_irc_protocol_line, GenericIrcEvent, GenericIrcSession, GenericIrcSessionCommand,
};
use crust_twitch::{
    eventsub::{EventSubCommand, EventSubEvent, EventSubNoticeKind, EventSubSession},
    parse_line, parse_privmsg_irc,
    session::client::{SessionCommand, TwitchEvent, TwitchSession},
};
use crust_ui::CrustApp;
use seventv::{
    apply_7tv_cosmetics_to_sender, load_7tv_cosmetics_catalog, load_7tv_user_style_for_twitch,
    resolve_7tv_user_style, SevenTvBadgeMeta, SevenTvCosmeticUpdate, SevenTvPaintMeta,
    SevenTvResolvedStyle, SevenTvUserStyleRaw,
};

use runtime::assets::fetch_emote_image;
use runtime::link_preview::fetch_link_preview;
use runtime::profiles::{
    fetch_ivr_logs, fetch_self_avatar, fetch_twitch_user_profile, fetch_user_profile_for_channel,
};
use runtime::system_messages::{
    build_sub_text, extract_irc_msg_echo, format_timeout_text, is_twitch_pinned_notice,
    make_system_message,
};

mod runtime;
mod seventv;

const CMD_CHANNEL_SIZE: usize = 128;
const EVT_CHANNEL_SIZE: usize = 4096;
const TWITCH_EVT_SIZE: usize = 4096;
const KICK_EVT_SIZE: usize = 4096;
const IRC_EVT_SIZE: usize = 4096;
const EVENTSUB_EVT_SIZE: usize = 1024;
const TWITCH_MAX_MESSAGE_CHARS: usize = 500;
const TWITCH_GQL_URL: &str = "https://gql.twitch.tv/gql";
const TWITCH_WEB_CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const APP_INITIAL_INNER_SIZE: [f32; 2] = [1100.0, 700.0];
const APP_MIN_INNER_SIZE: [f32; 2] = [220.0, 200.0];

static UI_REPAINT_CTX: OnceLock<egui::Context> = OnceLock::new();

#[inline]
fn request_ui_repaint() {
    if let Some(ctx) = UI_REPAINT_CTX.get() {
        ctx.request_repaint();
    }
}

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
    // "twitch" is checked last so third-party overrides take precedence,
    // but it ensures Twitch-native globals (e.g. LUL, Kappa) are still
    // resolved for local-echo messages that lack IRC emote-position tags.
    for provider in &["7tv", "bttv", "ffz", "kick", "twitch"] {
        if let Some(info) = idx.get(&emote_key(provider, code)) {
            return Some(info);
        }
    }
    None
}

/// Shared badge map: (scope, set_name, version) → image URL.
/// `scope` is `""` for global badges, or the channel name for channel-specific badges.
type BadgeMap = Arc<RwLock<std::collections::HashMap<(String, String, String), String>>>;

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

fn load_badge_map_cache_into(map: &BadgeMap) -> usize {
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

fn persist_badge_map_cache(map: &BadgeMap) {
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

    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, payload) {
        warn!("Failed writing temporary badge cache {:?}: {e}", tmp);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        warn!("Failed replacing badge cache {:?}: {e}", path);
    }
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
    let (ui_evt_tx, evt_rx) = mpsc::channel::<AppEvent>(EVT_CHANNEL_SIZE);
    // Runtime-side event bus. A bridge forwards events to the UI channel and
    // immediately requests an egui repaint so new events show up without delay.
    let (evt_tx, mut evt_bridge_rx) = mpsc::channel::<AppEvent>(EVT_CHANNEL_SIZE);

    // Twitch session channels
    let (tw_evt_tx, tw_evt_rx) = mpsc::channel::<TwitchEvent>(TWITCH_EVT_SIZE);
    let (sess_cmd_tx, sess_cmd_rx) = mpsc::channel::<SessionCommand>(64);

    // Kick session channels
    let (kick_evt_tx, kick_evt_rx) = mpsc::channel::<KickEvent>(KICK_EVT_SIZE);
    let (kick_cmd_tx, kick_cmd_rx) = mpsc::channel::<KickSessionCommand>(64);

    // Generic IRC session channels
    let (irc_evt_tx, irc_evt_rx) = mpsc::channel::<GenericIrcEvent>(IRC_EVT_SIZE);
    let (irc_cmd_tx, irc_cmd_rx) = mpsc::channel::<GenericIrcSessionCommand>(128);

    // Twitch EventSub channels
    let (eventsub_evt_tx, eventsub_evt_rx) = mpsc::channel::<EventSubEvent>(EVENTSUB_EVT_SIZE);
    let (eventsub_cmd_tx, eventsub_cmd_rx) = mpsc::channel::<EventSubCommand>(128);

    // Emote index shared between loaders and reducer
    let emote_index: EmoteIndex = Arc::new(RwLock::new(std::collections::HashMap::new()));

    // Track which emote codes are global (vs channel-specific)
    let global_emote_codes: Arc<RwLock<std::collections::HashSet<String>>> =
        Arc::new(RwLock::new(std::collections::HashSet::new()));

    // Emote cache for disk/network
    let emote_cache = EmoteCache::new().ok();

    // Badge map: (set, version) → URL
    let badge_map: BadgeMap = Arc::new(RwLock::new(std::collections::HashMap::new()));
    let loaded_cached_badges = load_badge_map_cache_into(&badge_map);
    if loaded_cached_badges > 0 {
        info!("Loaded {} badge mappings from local cache", loaded_cached_badges);
    }

    // Settings / token storage
    let settings_store = SettingsStore::new().ok();
    let chat_logs = LogStore::new().ok();
    let initial_settings: AppSettings = settings_store
        .as_ref()
        .map(|s| s.load())
        .unwrap_or_default();
    let kick_runtime_enabled = initial_settings.enable_kick_beta;
    let irc_runtime_enabled = initial_settings.enable_irc_beta;

    // Apply persisted theme before UI renders.
    crust_ui::theme::apply_from_str(&initial_settings.theme);

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

    rt.spawn(async move {
        while let Some(evt) = evt_bridge_rx.recv().await {
            if ui_evt_tx.send(evt).await.is_ok() {
                request_ui_repaint();
            }
        }
    });

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

    // Spawn Twitch EventSub websocket/session manager.
    rt.spawn({
        let session = EventSubSession::new(eventsub_evt_tx, eventsub_cmd_rx);
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
        let token = saved_token.clone();
        async move {
            load_global_badges(&bm, &cache, &etx, token).await;
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
            eventsub_evt_rx,
            evt_tx,
            sess_cmd_tx,
            kick_cmd_tx,
            irc_cmd_tx,
            eventsub_cmd_tx,
            idx,
            cache,
            bm,
            gc,
            settings_store,
            chat_logs,
            saved_token,
            kick_runtime_enabled,
            irc_runtime_enabled,
        )
    });

    // eframe / egui: UI framework initialization
    let native_opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Crust – Twitch, Kick & IRC Chat")
            .with_inner_size(APP_INITIAL_INNER_SIZE)
            .with_min_inner_size(APP_MIN_INNER_SIZE)
            .with_app_id("crust"),
        ..Default::default()
    };

    let result = eframe::run_native(
        "crust",
        native_opts,
        Box::new(move |cc| {
            let _ = UI_REPAINT_CTX.set(cc.egui_ctx.clone());
            Ok(Box::new(CrustApp::new(cc, cmd_tx, evt_rx)))
        }),
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
    mut eventsub_rx: mpsc::Receiver<EventSubEvent>,
    evt_tx: mpsc::Sender<AppEvent>,
    sess_tx: mpsc::Sender<SessionCommand>,
    kick_tx: mpsc::Sender<KickSessionCommand>,
    irc_tx: mpsc::Sender<GenericIrcSessionCommand>,
    eventsub_tx: mpsc::Sender<EventSubCommand>,
    emote_index: EmoteIndex,
    emote_cache: Option<EmoteCache>,
    badge_map: BadgeMap,
    global_emote_codes: GlobalCodes,
    settings_store: Option<SettingsStore>,
    chat_logs: Option<LogStore>,
    saved_token: Option<String>,
    kick_runtime_enabled: bool,
    irc_runtime_enabled: bool,
) {
    // Track URLs we've already queued for image download
    let mut pending_images: HashSet<String> = HashSet::new();
    // Track URLs we've already kicked off a link-preview fetch for.
    let mut pending_link_previews: HashSet<String> = HashSet::new();

    // 7TV cosmetics cache: global badges/paints + per-user resolved styles.
    let (stv_update_tx, mut stv_update_rx) = mpsc::channel::<SevenTvCosmeticUpdate>(512);
    let mut stv_badges: HashMap<String, SevenTvBadgeMeta> = HashMap::new();
    let mut stv_paints: HashMap<String, SevenTvPaintMeta> = HashMap::new();
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
            if let Some((badges, paints)) = load_7tv_cosmetics_catalog(&client).await {
                let _ = tx
                    .send(SevenTvCosmeticUpdate::Catalog { badges, paints })
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
            let _ = tx
                .send(TokenValidationResult::Startup { token, result })
                .await;
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
    let _ = evt_tx
        .send(AppEvent::ChatUiBehaviorUpdated {
            prevent_overlong_twitch_messages: settings.prevent_overlong_twitch_messages,
            collapse_long_messages: settings.collapse_long_messages,
            collapse_long_message_lines: settings.collapse_long_message_lines,
            animations_when_focused: settings.animations_when_focused,
        })
        .await;
    let _ = evt_tx
        .send(AppEvent::GeneralSettingsUpdated {
            show_timestamps: settings.show_timestamps,
            auto_join: settings.auto_join.clone(),
            highlights: settings.highlights.clone(),
            ignores: settings.ignores.clone(),
        })
        .await;
    let _ = evt_tx
        .send(AppEvent::AppearanceSettingsUpdated {
            channel_layout: settings.channel_layout.clone(),
            sidebar_visible: settings.sidebar_visible,
            analytics_visible: settings.analytics_visible,
            irc_status_visible: settings.irc_status_visible,
            tab_style: settings.tab_style.clone(),
            show_tab_close_buttons: settings.show_tab_close_buttons,
            show_tab_live_indicators: settings.show_tab_live_indicators,
            split_header_show_title: settings.split_header_show_title,
            split_header_show_game: settings.split_header_show_game,
            split_header_show_viewer_count: settings.split_header_show_viewer_count,
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
                        let _ = eventsub_tx
                            .send(EventSubCommand::WatchChannel {
                                broadcaster_id: room_id.clone(),
                            })
                            .await;

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
                        let ch_badge = channel.0.clone();
                        let badge_token = (!settings.oauth_token.trim().is_empty())
                            .then(|| settings.oauth_token.clone());
                        tokio::spawn(async move {
                            load_channel_badges(
                                &room_id,
                                &ch_badge,
                                &bm,
                                &cache_b,
                                &etx,
                                badge_token,
                            )
                            .await;
                        });
                        // Load persisted local chat history first (SQLite),
                        // then merge remote history on top.
                        if let Some(store) = chat_logs.clone() {
                            let ch_local = channel.clone();
                            let etx_local = evt_tx.clone();
                            tokio::spawn(async move {
                                load_local_recent_messages(ch_local, store, etx_local).await;
                            });
                        }
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
                        // Fetch the currently pinned message snapshot for this channel.
                        // Twitch does not always replay moderator-pinned state over IRC
                        // when joining, so we mirror the web client GraphQL query.
                        let ch_pin = channel.clone();
                        let etx_pin = evt_tx.clone();
                        tokio::spawn(async move {
                            fetch_current_twitch_pinned_message(ch_pin, etx_pin).await;
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
                        sync_eventsub_auth(
                            &eventsub_tx,
                            &settings,
                            helix_client_id.as_deref(),
                            Some(&user_id),
                        )
                        .await;

                        for broadcaster_id in channel_room_ids.values() {
                            let _ = eventsub_tx
                                .send(EventSubCommand::WatchChannel {
                                    broadcaster_id: broadcaster_id.clone(),
                                })
                                .await;
                        }

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
                                badge.url = resolve_badge_url(&bm, &msg.channel.0, &badge.name, &badge.version);
                            }
                        }

                        // Mention / reply-to-me detection
                        if let Some(ref uname) = auth_username {
                            let uname_lower = uname.to_lowercase();
                            let text_lower = msg.raw_text.to_lowercase();
                            // Direct @mention in message body
                            let has_at_mention = text_lower
                                .contains(&format!("@{uname_lower}"));
                            // Bare username as a whole word
                            let has_bare_mention = text_lower
                                .split(|c: char| !c.is_alphanumeric() && c != '_')
                                .any(|w| w == uname_lower);
                            // Reply directed at us
                            let is_reply_to_me = msg.reply.as_ref()
                                .map(|r| r.parent_user_login.to_lowercase() == uname_lower)
                                .unwrap_or(false);
                            msg.flags.is_mention = has_at_mention || has_bare_mention || is_reply_to_me;
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
                        if let Some(store) = chat_logs.as_ref() {
                            if let Err(e) = store.append_message(&msg) {
                                warn!("chat-log: failed to persist Twitch message: {e}");
                            }
                        }
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
                                let mut msg = make_system_message(
                                    local_msg_id, ch, notice.text.clone(), notice.timestamp,
                                    MsgKind::SystemInfo,
                                );
                                if is_twitch_pinned_notice(&notice.text) {
                                    msg.flags.is_pinned = true;
                                }
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
                                badge.url = resolve_badge_url(&bm, &channel.0, &badge.name, &badge.version);
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
                        if let Some(store) = chat_logs.as_ref() {
                            if let Err(e) = store.append_message(&msg) {
                                warn!("chat-log: failed to persist Kick message: {e}");
                            }
                        }
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
                            let text_lower = msg.raw_text.to_lowercase();
                            // @mention or bare username as a whole word
                            has_mention = text_lower
                                .contains(&format!("@{uname_lower}"))
                                || text_lower
                                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                                    .any(|w| w == uname_lower);
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
                        if let Some(store) = chat_logs.as_ref() {
                            if let Err(e) = store.append_message(&msg) {
                                warn!("chat-log: failed to persist IRC message: {e}");
                            }
                        }
                        let _ = evt_tx.send(AppEvent::MessageReceived {
                            channel,
                            message: msg,
                        }).await;
                    }
                }
            }

            // Twitch EventSub notifications and reconnect/backfill signals.
            Some(eventsub_evt) = eventsub_rx.recv() => {
                match eventsub_evt {
                    EventSubEvent::Connected { resumed } => {
                        info!("EventSub connected (resumed={resumed})");
                    }
                    EventSubEvent::Reconnecting { attempt } => {
                        debug!("EventSub reconnect attempt {attempt}");
                    }
                    EventSubEvent::BackfillRequested => {
                        // Refresh per-channel profile snapshots after reconnect so
                        // online/offline title/game UI catches up immediately.
                        let twitch_channels: Vec<String> = channel_room_ids
                            .keys()
                            .filter(|ch| ch.is_twitch())
                            .map(|ch| ch.display_name().to_owned())
                            .collect();
                        for login in twitch_channels {
                            let etx = evt_tx.clone();
                            tokio::spawn(async move {
                                fetch_twitch_user_profile(&login, etx).await;
                            });
                        }
                    }
                    EventSubEvent::Notice(notice) => {
                        let channel = channel_room_ids
                            .iter()
                            .find(|(_, room_id)| *room_id == &notice.broadcaster_id)
                            .map(|(ch, _)| ch.clone())
                            .or_else(|| {
                                notice
                                    .broadcaster_login
                                    .as_ref()
                                    .map(|login| ChannelId::new(login.clone()))
                            });

                        if let Some(channel) = channel {
                            let (msg_kind, text) = eventsub_notice_to_message(&notice.kind);
                            let msg = make_system_message(
                                local_msg_id,
                                channel.clone(),
                                text,
                                Utc::now(),
                                msg_kind,
                            );
                            local_msg_id += 1;
                            let _ = evt_tx
                                .send(AppEvent::MessageReceived {
                                    channel,
                                    message: msg,
                                })
                                .await;
                        }
                    }
                    EventSubEvent::Error(message) => {
                        warn!("EventSub error: {message}");
                        let _ = evt_tx
                            .send(AppEvent::Error {
                                context: "EventSub".into(),
                                message,
                            })
                            .await;
                    }
                }
            }

            // Internal 7TV cosmetics updates (catalog + per-user style lookups).
            Some(stv_update) = stv_update_rx.recv() => {
                match stv_update {
                    SevenTvCosmeticUpdate::Catalog { badges, paints } => {
                        info!(
                            "7TV catalog received: {} badges, {} paints",
                            badges.len(),
                            paints.len()
                        );
                        stv_badges = badges;
                        stv_paints = paints;

                        // Re-resolve any styles we already learned before
                        // the cosmetics catalog was available.
                        stv_user_styles_resolved.clear();
                        let mut updates: Vec<(String, SevenTvResolvedStyle)> = Vec::new();
                        for (uid, style) in &stv_user_styles_raw {
                            let resolved = resolve_7tv_user_style(style, &stv_badges, &stv_paints);
                            updates.push((uid.clone(), resolved.clone()));
                            stv_user_styles_resolved.insert(uid.clone(), resolved);
                        }

                        if !updates.is_empty() {
                            info!("7TV re-resolved {} cached user styles", updates.len());
                        }

                        for (user_id, resolved) in updates {
                            if user_id.is_empty() {
                                continue;
                            }
                            let _ = evt_tx
                                .send(AppEvent::SenderCosmeticsUpdated {
                                    user_id,
                                    color: resolved.color_hex,
                                    name_paint: None,
                                    badge: resolved.badge,
                                    avatar_url: resolved.avatar_url,
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
                            let resolved = resolve_7tv_user_style(&raw, &stv_badges, &stv_paints);
                            stv_user_styles_raw.insert(twitch_user_id.clone(), raw);
                            stv_user_styles_resolved
                                .insert(twitch_user_id.clone(), resolved.clone());

                            if !twitch_user_id.is_empty() {
                                let _ = evt_tx
                                    .send(AppEvent::SenderCosmeticsUpdated {
                                        user_id: twitch_user_id,
                                        color: resolved.color_hex,
                                        name_paint: None,
                                        badge: resolved.badge,
                                        avatar_url: resolved.avatar_url,
                                    })
                                    .await;
                            }
                        }
                    }
                    SevenTvCosmeticUpdate::BatchUserLookup { user_ids } => {
                        // Triggered by history loading - queue lookups for
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
                        if !channel.is_twitch() {
                            if let Some(store) = chat_logs.clone() {
                                let etx_local = evt_tx.clone();
                                let ch_local = channel.clone();
                                tokio::spawn(async move {
                                    load_local_recent_messages(ch_local, store, etx_local).await;
                                });
                            }
                        }
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
                        if let Some(store) = chat_logs.clone() {
                            let etx_local = evt_tx.clone();
                            let ch_local = channel.clone();
                            tokio::spawn(async move {
                                load_local_recent_messages(ch_local, store, etx_local).await;
                            });
                        }
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
                        if let Some(broadcaster_id) = channel_room_ids.remove(&channel) {
                            let _ = eventsub_tx
                                .send(EventSubCommand::UnwatchChannel { broadcaster_id })
                                .await;
                        }
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
                        let _ = eventsub_tx.send(EventSubCommand::ClearAuth).await;
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
                            let _ = eventsub_tx.send(EventSubCommand::ClearAuth).await;
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
                                    let _ = eventsub_tx.send(EventSubCommand::ClearAuth).await;
                                    let _ = sess_tx.send(SessionCommand::LogoutAndReconnect).await;
                                    let _ = evt_tx.send(AppEvent::LoggedOut).await;
                                }
                            } else {
                                auth_username = None;
                                auth_user_id = None;
                                settings.username = String::new();
                                let _ = eventsub_tx.send(EventSubCommand::ClearAuth).await;
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
                    AppCommand::SetTheme { theme } => {
                        settings.theme = theme;
                        if let Some(store) = &settings_store {
                            if let Err(e) = store.save(&settings) {
                                warn!("Failed to save theme setting: {e}");
                            }
                        }
                    }
                    AppCommand::SetChatUiBehavior {
                        prevent_overlong_twitch_messages,
                        collapse_long_messages,
                        collapse_long_message_lines,
                        animations_when_focused,
                    } => {
                        settings.prevent_overlong_twitch_messages =
                            prevent_overlong_twitch_messages;
                        settings.collapse_long_messages = collapse_long_messages;
                        settings.collapse_long_message_lines = collapse_long_message_lines.max(1);
                        settings.animations_when_focused = animations_when_focused;
                        if let Some(store) = &settings_store {
                            if let Err(e) = store.save(&settings) {
                                warn!("Failed to save chat UI behavior settings: {e}");
                            }
                        }
                        let _ = evt_tx
                            .send(AppEvent::ChatUiBehaviorUpdated {
                                prevent_overlong_twitch_messages:
                                    settings.prevent_overlong_twitch_messages,
                                collapse_long_messages: settings.collapse_long_messages,
                                collapse_long_message_lines: settings.collapse_long_message_lines,
                                animations_when_focused: settings.animations_when_focused,
                            })
                            .await;
                    }
                    AppCommand::SetGeneralSettings {
                        show_timestamps,
                        auto_join,
                        highlights,
                        ignores,
                    } => {
                        let mut seen_channels: HashSet<String> = HashSet::new();
                        let mut sanitized_auto_join: Vec<String> = Vec::new();
                        for raw in auto_join {
                            if let Some(id) = parse_saved_channel(&raw) {
                                if seen_channels.insert(id.0.clone()) {
                                    sanitized_auto_join.push(id.0);
                                }
                            }
                        }
                        sanitized_auto_join.sort();

                        let mut seen_highlights: HashSet<String> = HashSet::new();
                        let mut sanitized_highlights: Vec<String> = Vec::new();
                        for raw in highlights {
                            let trimmed = raw.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            let key = trimmed.to_ascii_lowercase();
                            if seen_highlights.insert(key) {
                                sanitized_highlights.push(trimmed.to_owned());
                            }
                        }

                        let mut seen_ignores: HashSet<String> = HashSet::new();
                        let mut sanitized_ignores: Vec<String> = Vec::new();
                        for raw in ignores {
                            let trimmed = raw.trim().to_ascii_lowercase();
                            if trimmed.is_empty() {
                                continue;
                            }
                            if seen_ignores.insert(trimmed.clone()) {
                                sanitized_ignores.push(trimmed);
                            }
                        }

                        settings.show_timestamps = show_timestamps;
                        settings.auto_join = sanitized_auto_join.clone();
                        settings.highlights = sanitized_highlights.clone();
                        settings.ignores = sanitized_ignores.clone();
                        joined_channels = settings.auto_join.iter().cloned().collect();

                        if let Some(store) = &settings_store {
                            if let Err(e) = store.save(&settings) {
                                warn!("Failed to save general settings: {e}");
                            }
                        }

                        let _ = evt_tx
                            .send(AppEvent::GeneralSettingsUpdated {
                                show_timestamps: settings.show_timestamps,
                                auto_join: settings.auto_join.clone(),
                                highlights: settings.highlights.clone(),
                                ignores: settings.ignores.clone(),
                            })
                            .await;
                    }
                    AppCommand::SetAppearanceSettings {
                        channel_layout,
                        sidebar_visible,
                        analytics_visible,
                        irc_status_visible,
                        tab_style,
                        show_tab_close_buttons,
                        show_tab_live_indicators,
                        split_header_show_title,
                        split_header_show_game,
                        split_header_show_viewer_count,
                    } => {
                        settings.channel_layout = if matches!(
                            channel_layout.as_str(),
                            "sidebar" | "top_tabs"
                        ) {
                            channel_layout
                        } else {
                            "sidebar".to_owned()
                        };
                        settings.sidebar_visible = sidebar_visible;
                        settings.analytics_visible = analytics_visible;
                        settings.irc_status_visible = irc_status_visible;
                        settings.tab_style = if matches!(tab_style.as_str(), "compact" | "normal")
                        {
                            tab_style
                        } else {
                            "compact".to_owned()
                        };
                        settings.show_tab_close_buttons = show_tab_close_buttons;
                        settings.show_tab_live_indicators = show_tab_live_indicators;
                        settings.split_header_show_title = split_header_show_title;
                        settings.split_header_show_game = split_header_show_game;
                        settings.split_header_show_viewer_count =
                            split_header_show_viewer_count;
                        if let Some(store) = &settings_store {
                            if let Err(e) = store.save(&settings) {
                                warn!("Failed to save appearance settings: {e}");
                            }
                        }
                        let _ = evt_tx
                            .send(AppEvent::AppearanceSettingsUpdated {
                                channel_layout: settings.channel_layout.clone(),
                                sidebar_visible: settings.sidebar_visible,
                                analytics_visible: settings.analytics_visible,
                                irc_status_visible: settings.irc_status_visible,
                                tab_style: settings.tab_style.clone(),
                                show_tab_close_buttons: settings.show_tab_close_buttons,
                                show_tab_live_indicators: settings.show_tab_live_indicators,
                                split_header_show_title: settings.split_header_show_title,
                                split_header_show_game: settings.split_header_show_game,
                                split_header_show_viewer_count:
                                    settings.split_header_show_viewer_count,
                            })
                            .await;
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
                                    name_paint: None,
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
                                    is_pinned: false,
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
                                        name_paint: None,
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
                                        is_pinned: false,
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
                    AppCommand::UpdateRewardRedemptionStatus {
                        channel,
                        reward_id,
                        redemption_id,
                        status,
                        user_login,
                        reward_title,
                    } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        let evt_tx2 = evt_tx.clone();
                        let ch_name = channel.clone();
                        tokio::spawn(async move {
                            helix_update_reward_redemption_status(
                                &token,
                                client_id.as_deref(),
                                broadcaster_id.as_deref(),
                                &reward_id,
                                &redemption_id,
                                &status,
                                &user_login,
                                &reward_title,
                                &ch_name,
                                evt_tx2,
                            )
                            .await;
                        });
                    }
                    AppCommand::CreatePoll {
                        channel,
                        title,
                        choices,
                        duration_secs,
                    } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        let evt_tx2 = evt_tx.clone();
                        tokio::spawn(async move {
                            helix_create_poll(
                                &token,
                                client_id.as_deref(),
                                broadcaster_id.as_deref(),
                                &title,
                                &choices,
                                duration_secs,
                                &channel,
                                evt_tx2,
                            )
                            .await;
                        });
                    }
                    AppCommand::EndPoll { channel, status } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        let evt_tx2 = evt_tx.clone();
                        tokio::spawn(async move {
                            helix_end_poll(
                                &token,
                                client_id.as_deref(),
                                broadcaster_id.as_deref(),
                                &status,
                                &channel,
                                evt_tx2,
                            )
                            .await;
                        });
                    }
                    AppCommand::CreatePrediction {
                        channel,
                        title,
                        outcomes,
                        duration_secs,
                    } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        let evt_tx2 = evt_tx.clone();
                        tokio::spawn(async move {
                            helix_create_prediction(
                                &token,
                                client_id.as_deref(),
                                broadcaster_id.as_deref(),
                                &title,
                                &outcomes,
                                duration_secs,
                                &channel,
                                evt_tx2,
                            )
                            .await;
                        });
                    }
                    AppCommand::LockPrediction { channel } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        let evt_tx2 = evt_tx.clone();
                        tokio::spawn(async move {
                            helix_lock_prediction(
                                &token,
                                client_id.as_deref(),
                                broadcaster_id.as_deref(),
                                &channel,
                                evt_tx2,
                            )
                            .await;
                        });
                    }
                    AppCommand::ResolvePrediction {
                        channel,
                        winning_outcome_index,
                    } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        let evt_tx2 = evt_tx.clone();
                        tokio::spawn(async move {
                            helix_resolve_prediction(
                                &token,
                                client_id.as_deref(),
                                broadcaster_id.as_deref(),
                                winning_outcome_index,
                                &channel,
                                evt_tx2,
                            )
                            .await;
                        });
                    }
                    AppCommand::CancelPrediction { channel } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        let evt_tx2 = evt_tx.clone();
                        tokio::spawn(async move {
                            helix_cancel_prediction(
                                &token,
                                client_id.as_deref(),
                                broadcaster_id.as_deref(),
                                &channel,
                                evt_tx2,
                            )
                            .await;
                        });
                    }
                    AppCommand::StartCommercial {
                        channel,
                        length_secs,
                    } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        let evt_tx2 = evt_tx.clone();
                        tokio::spawn(async move {
                            helix_start_commercial(
                                &token,
                                client_id.as_deref(),
                                broadcaster_id.as_deref(),
                                length_secs,
                                &channel,
                                evt_tx2,
                            )
                            .await;
                        });
                    }
                    AppCommand::CreateStreamMarker {
                        channel,
                        description,
                    } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        let evt_tx2 = evt_tx.clone();
                        tokio::spawn(async move {
                            helix_create_stream_marker(
                                &token,
                                client_id.as_deref(),
                                broadcaster_id.as_deref(),
                                description.as_deref(),
                                &channel,
                                evt_tx2,
                            )
                            .await;
                        });
                    }
                    AppCommand::SendAnnouncement {
                        channel,
                        message,
                        color,
                    } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let moderator_id = auth_user_id.clone();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        let evt_tx2 = evt_tx.clone();
                        tokio::spawn(async move {
                            helix_send_announcement(
                                &token,
                                client_id.as_deref(),
                                broadcaster_id.as_deref(),
                                moderator_id.as_deref(),
                                &message,
                                color.as_deref(),
                                &channel,
                                evt_tx2,
                            )
                            .await;
                        });
                    }
                    AppCommand::SendShoutout {
                        channel,
                        target_login,
                    } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let moderator_id = auth_user_id.clone();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        let evt_tx2 = evt_tx.clone();
                        tokio::spawn(async move {
                            helix_send_shoutout(
                                &token,
                                client_id.as_deref(),
                                broadcaster_id.as_deref(),
                                moderator_id.as_deref(),
                                &target_login,
                                &channel,
                                evt_tx2,
                            )
                            .await;
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
                    AppCommand::FetchIvrLogs { channel, username } => {
                        let etx = evt_tx.clone();
                        tokio::spawn(async move {
                            fetch_ivr_logs(&channel, &username, etx).await;
                        });
                    }
                    AppCommand::LoadOlderLocalHistory {
                        channel,
                        before_ts_ms,
                        limit,
                    } => {
                        let Some(store) = chat_logs.clone() else {
                            let _ = evt_tx
                                .send(AppEvent::Error {
                                    context: "History".into(),
                                    message: "Local history is unavailable on this system.".into(),
                                })
                                .await;
                            continue;
                        };
                        let etx = evt_tx.clone();
                        tokio::spawn(async move {
                            load_local_older_messages(channel, before_ts_ms, limit, store, etx)
                                .await;
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
                                sync_eventsub_auth(
                                    &eventsub_tx,
                                    &settings,
                                    helix_client_id.as_deref(),
                                    auth_user_id.as_deref(),
                                )
                                .await;
                                // auth_in_progress was already set to true before spawn
                                let _ = sess_tx
                                    .send(SessionCommand::Authenticate { token, nick: login })
                                    .await;
                            }
                            Err(ValidateError::Unauthorized) => {
                                warn!("Saved token rejected by Twitch, clearing and starting anonymous");
                                auth_in_progress = false;
                                let _ = eventsub_tx.send(EventSubCommand::ClearAuth).await;
                                if let Some(store) = &settings_store {
                                    let _ = store.delete_token();
                                }
                                let _ = evt_tx.send(AppEvent::LoggedOut).await;
                            }
                            Err(ValidateError::Transient(e)) => {
                                warn!("Token validation failed ({e}), keeping token and starting anonymous");
                                auth_in_progress = false;
                                let _ = eventsub_tx.send(EventSubCommand::ClearAuth).await;
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
                                let _ = eventsub_tx.send(EventSubCommand::ClearAuth).await;
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
                                let _ = eventsub_tx.send(EventSubCommand::ClearAuth).await;
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

async fn sync_eventsub_auth(
    eventsub_tx: &mpsc::Sender<EventSubCommand>,
    settings: &AppSettings,
    helix_client_id: Option<&str>,
    auth_user_id: Option<&str>,
) {
    let token = settings.oauth_token.trim();
    let cid = helix_client_id.unwrap_or("").trim();
    let uid = auth_user_id.unwrap_or("").trim();

    if token.is_empty() || cid.is_empty() || uid.is_empty() {
        let _ = eventsub_tx.send(EventSubCommand::ClearAuth).await;
        return;
    }

    let _ = eventsub_tx
        .send(EventSubCommand::SetAuth {
            token: token.to_owned(),
            client_id: cid.to_owned(),
            user_id: uid.to_owned(),
        })
        .await;
}

fn format_eventsub_notice_text(kind: &EventSubNoticeKind) -> String {
    match kind {
        EventSubNoticeKind::Follow { user_login } => {
            format!("{user_login} followed the channel.")
        }
        EventSubNoticeKind::Subscribe {
            user_login,
            tier,
            is_gift,
        } => {
            if *is_gift {
                format!("{user_login} subscribed with a gifted {tier} sub.")
            } else {
                format!("{user_login} subscribed ({tier}).")
            }
        }
        EventSubNoticeKind::SubscriptionGift {
            gifter_login,
            tier,
            total,
        } => {
            let from = gifter_login
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("An anonymous gifter");
            if let Some(total) = total {
                format!("{from} gifted {total} {tier} subscriptions.")
            } else {
                format!("{from} gifted a {tier} subscription.")
            }
        }
        EventSubNoticeKind::Raid {
            from_login,
            viewers,
        } => {
            format!("Incoming raid from {from_login} with {viewers} viewers.")
        }
        EventSubNoticeKind::ChannelPointsRedemption {
            user_login,
            reward_title,
            cost,
            user_input,
            status,
            ..
        } => {
            let mut out = format!(
                "{user_login} redeemed '{reward_title}' ({} points)",
                cost
            );
            if let Some(input) = user_input.as_deref().filter(|s| !s.trim().is_empty()) {
                out.push_str(&format!(": {input}"));
            }
            if let Some(status) = status.as_deref().filter(|s| !s.trim().is_empty()) {
                out.push_str(&format!(" [{status}]"));
            }
            out
        }
        EventSubNoticeKind::PollLifecycle {
            title,
            phase,
            status,
        } => {
            if let Some(status) = status.as_deref().filter(|s| !s.is_empty()) {
                format!("Poll {phase}: {title} ({status})")
            } else {
                format!("Poll {phase}: {title}")
            }
        }
        EventSubNoticeKind::PredictionLifecycle {
            title,
            phase,
            status,
        } => {
            if let Some(status) = status.as_deref().filter(|s| !s.is_empty()) {
                format!("Prediction {phase}: {title} ({status})")
            } else {
                format!("Prediction {phase}: {title}")
            }
        }
        EventSubNoticeKind::StreamOnline => "Stream is now live.".to_owned(),
        EventSubNoticeKind::StreamOffline => "Stream is now offline.".to_owned(),
    }
}

fn eventsub_notice_to_message(kind: &EventSubNoticeKind) -> (MsgKind, String) {
    match kind {
        EventSubNoticeKind::ChannelPointsRedemption {
            user_login,
            reward_title,
            cost,
            reward_id,
            redemption_id,
            user_input,
            status,
        } => {
            let text = format_eventsub_notice_text(kind);
            (
                MsgKind::ChannelPointsReward {
                    user_login: user_login.clone(),
                    reward_title: reward_title.clone(),
                    cost: *cost,
                    reward_id: reward_id.clone(),
                    redemption_id: redemption_id.clone(),
                    user_input: user_input.clone(),
                    status: status.clone(),
                },
                text,
            )
        }
        _ => (MsgKind::SystemInfo, format_eventsub_notice_text(kind)),
    }
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
    let twg = TwitchGlobalProvider::new();

    let (b, f, s, t) = tokio::join!(
        bttv.load_global(),
        ffz.load_global(),
        stv.load_global(),
        twg.load_global(),
    );

    let total = b.len() + f.len() + s.len() + t.len();
    info!(
        "Loaded {total} global emotes (BTTV={}, FFZ={}, 7TV={}, Twitch={})",
        b.len(),
        f.len(),
        s.len(),
        t.len(),
    );

    // Collect URLs of the newly-loaded emotes for prefetching.
    let new_urls: Vec<String> = f
        .iter()
        .chain(b.iter())
        .chain(s.iter())
        .chain(t.iter())
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
        for e in t {
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

// Badge loading

/// Chatterino's bundled Twitch global badge dataset, used as a reliable
/// fallback when live badge APIs are unavailable or return an unexpected shape.
const CHATTERINO_TWITCH_BADGES_JSON: &str =
    include_str!("../../../chatterino2-master/resources/twitch-badges.json");

/// Resolve a badge image URL from the badge map.
///
/// Twitch IRC sends some badge versions as cumulative counts (e.g.
/// `subscriber/28`, `bits/5000`) that don't directly match the fixed tier
/// version keys stored by the badge API (e.g. `"0"`, `"3"`, `"6"`, `"24"`).
/// When an exact match is not found this function falls back to the highest
/// available version that is numerically ≤ the requested version, which
/// selects the correct tier image without requiring a perfect key match.
fn resolve_badge_url(
    map: &std::collections::HashMap<(String, String, String), String>,
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
        // Slow path: numeric fallback - find the highest available version ≤ version.
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
    map: &mut std::collections::HashMap<(String, String, String), String>,
) {
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
    struct ChatterinoBadgeVersion {
        id: String,
        image: String,
    }

    fn normalize_badge_url(url: String) -> String {
        let url = if url.starts_with("//") {
            format!("https:{url}")
        } else {
            url
        };

        // Some badge datasets (including Chatterino's bundled JSON) use
        // base URLs that end in `/` and require a scale suffix (`/1`, `/2`, `/3`).
        // Use `/3` (closest to Chatterino's high-res badge rendering).
        if url.contains("/badges/v1/") {
            let trimmed = url.trim_end_matches('/');
            let tail = trimmed.rsplit('/').next().unwrap_or("");
            let has_explicit_scale = matches!(tail, "1" | "2" | "3" | "4");
            if !has_explicit_scale {
                return format!("{trimmed}/3");
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
        return;
    }

    // Chatterino/bundled shape: { "set_id": [{ id, image, ... }, ...], ... }
    if let Ok(sets) =
        serde_json::from_str::<std::collections::HashMap<String, Vec<ChatterinoBadgeVersion>>>(
            body,
        )
    {
        for (set_id, versions) in sets {
            for ver in versions {
                let url = normalize_badge_url(ver.image);
                for alias in badge_set_aliases(&set_id) {
                    map.insert((scope.clone(), alias, ver.id.clone()), url.clone());
                }
            }
        }
    }
}

/// Parse Twitch legacy badges.v1 response and insert into the badge map.
///
/// Shape:
/// `{ "badge_sets": { "set_id": { "versions": { "1": { image_url_*x }}}}}`
fn parse_badges_v1_response(
    body: &str,
    scope: &str,
    map: &mut std::collections::HashMap<(String, String, String), String>,
) {
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
        versions: std::collections::HashMap<String, Version>,
    }

    #[derive(serde::Deserialize)]
    struct Payload {
        badge_sets: std::collections::HashMap<String, BadgeSet>,
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
        Err(_) => return,
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
}

fn parse_helix_badge_response(
    body: &str,
    scope: &str,
    map: &mut std::collections::HashMap<(String, String, String), String>,
) {
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
        Err(_) => return,
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
    match validate_token(&bearer).await {
        Ok(info) => Some((info.client_id, bearer)),
        Err(e) => {
            warn!("Helix auth setup failed while validating token: {e}");
            None
        }
    }
}

async fn load_global_badges_v1_fallback(
    badge_map: &BadgeMap,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
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
        let new_urls = {
            let mut map = badge_map.write().unwrap();
            let before: std::collections::HashSet<String> = map.values().cloned().collect();
            parse_badges_v1_response(&text, "", &mut map);
            map.values()
                .filter(|u| !before.contains(*u))
                .cloned()
                .collect::<Vec<_>>()
        };
        if !new_urls.is_empty() {
            info!(
                "Loaded {} global badges via badges.twitch.tv fallback",
                new_urls.len()
            );
            prefetch_badge_images(new_urls, cache, evt_tx);
            persist_badge_map_cache(badge_map);
        }
    }
}

async fn load_channel_badges_v1_fallback(
    room_id: &str,
    channel: &str,
    badge_map: &BadgeMap,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    if !host_resolves("badges.twitch.tv").await {
        warn_badges_twitch_unresolved_once();
        return;
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
        let new_urls = {
            let mut map = badge_map.write().unwrap();
            let before: std::collections::HashSet<String> = map.values().cloned().collect();
            parse_badges_v1_response(&text, channel, &mut map);
            map.values()
                .filter(|u| !before.contains(*u))
                .cloned()
                .collect::<Vec<_>>()
        };
        if !new_urls.is_empty() {
            info!(
                "Loaded {} channel badges for room {room_id} via badges.twitch.tv fallback",
                new_urls.len()
            );
            prefetch_badge_images(new_urls, cache, evt_tx);
            persist_badge_map_cache(badge_map);
        }
    }
}

/// Load global Twitch badges via IVR API (no auth required).
async fn load_global_badges(
    badge_map: &BadgeMap,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
    oauth_token: Option<String>,
) {
    // Seed global badges from Chatterino's bundled catalog first so badge
    // images still work when live API calls fail.
    let bundled_urls = {
        let mut map = badge_map.write().unwrap();
        let before: std::collections::HashSet<String> = map.values().cloned().collect();
        parse_ivr_badge_response(CHATTERINO_TWITCH_BADGES_JSON, "", &mut map);
        map.values()
            .filter(|u| !before.contains(*u))
            .cloned()
            .collect::<Vec<_>>()
    };
    if !bundled_urls.is_empty() {
        info!(
            "Loaded {} global badges from bundled Chatterino dataset",
            bundled_urls.len()
        );
        prefetch_badge_images(bundled_urls, cache, evt_tx);
        persist_badge_map_cache(badge_map);
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("crust-badges/1.0")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let helix_auth = helix_auth_from_token(oauth_token.as_deref()).await;
    if let Some((ref client_id, ref token)) = helix_auth {
        if let Some(text) = fetch_badge_payload_with_retries(
            &client,
            &["https://api.twitch.tv/helix/chat/badges/global"],
            "Helix global badges",
            Some((client_id.as_str(), token.as_str())),
        )
        .await
        {
            let new_urls = {
                let mut map = badge_map.write().unwrap();
                let before: std::collections::HashSet<String> = map.values().cloned().collect();
                parse_helix_badge_response(&text, "", &mut map);
                let after_count = map.len();
                let new: Vec<String> = map
                    .values()
                    .filter(|u| !before.contains(*u))
                    .cloned()
                    .collect();
                info!("Loaded {} global badges via Helix", after_count - before.len());
                new
            };
            prefetch_badge_images(new_urls, cache, evt_tx);
            persist_badge_map_cache(badge_map);
            return;
        }
    }

    if let Some(text) = fetch_badge_payload_with_retries(
        &client,
        &["https://api.ivr.fi/v2/twitch/badges/global"],
        "IVR global badges",
        None,
    )
    .await
    {
        let new_urls = {
            let mut map = badge_map.write().unwrap();
            let before: std::collections::HashSet<String> = map.values().cloned().collect();
            parse_ivr_badge_response(&text, "", &mut map);
            let after_count = map.len();
            let new: Vec<String> = map
                .values()
                .filter(|u| !before.contains(*u))
                .cloned()
                .collect();
            info!("Loaded {} global badges via IVR", after_count - before.len());
            new
        };
        prefetch_badge_images(new_urls, cache, evt_tx);
        persist_badge_map_cache(badge_map);
    } else {
        load_global_badges_v1_fallback(badge_map, cache, evt_tx).await;
    }
}

/// Load channel-specific Twitch badges via IVR API (no auth required).
async fn load_channel_badges(
    room_id: &str,
    channel: &str,
    badge_map: &BadgeMap,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
    oauth_token: Option<String>,
) {
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
            let new_urls = {
                let mut map = badge_map.write().unwrap();
                let before: std::collections::HashSet<String> = map.values().cloned().collect();
                parse_helix_badge_response(&text, channel, &mut map);
                let new: Vec<String> = map
                    .values()
                    .filter(|u| !before.contains(*u))
                    .cloned()
                    .collect();
                info!(
                    "Loaded {} channel badges for room {room_id} via Helix",
                    new.len()
                );
                new
            };
            prefetch_badge_images(new_urls, cache, evt_tx);
            persist_badge_map_cache(badge_map);
            return;
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
        let new_urls = {
            let mut map = badge_map.write().unwrap();
            let before: std::collections::HashSet<String> = map.values().cloned().collect();
            parse_ivr_badge_response(&text, channel, &mut map);
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
        persist_badge_map_cache(badge_map);
    } else {
        load_channel_badges_v1_fallback(room_id, channel, badge_map, cache, evt_tx).await;
    }
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
        warn!(
            "Skipping badges.twitch.tv fallback: host is not resolvable in this environment"
        );
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

// Recent message history

/// Load locally persisted chat history from SQLite and replay it as
/// `AppEvent::HistoryLoaded` for the channel.
async fn load_local_recent_messages(
    channel: ChannelId,
    log_store: LogStore,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let channel_for_query = channel.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        log_store.recent_messages(&channel_for_query, LogStore::default_recent_limit())
    })
    .await;

    let mut messages = match loaded {
        Ok(Ok(rows)) => rows,
        Ok(Err(e)) => {
            warn!(
                "chat-history: local SQLite load failed for #{}: {e}",
                channel.display_name()
            );
            return;
        }
        Err(e) => {
            warn!(
                "chat-history: local SQLite task failed for #{}: {e}",
                channel.display_name()
            );
            return;
        }
    };

    if messages.is_empty() {
        return;
    }

    for msg in &mut messages {
        msg.id = MessageId(HISTORY_MSG_ID.fetch_sub(1, std::sync::atomic::Ordering::Relaxed));
        msg.flags.is_history = true;
        msg.channel = channel.clone();
    }

    info!(
        "chat-history: loaded {} local SQLite messages for #{}",
        messages.len(),
        channel.display_name()
    );
    let _ = evt_tx
        .send(AppEvent::HistoryLoaded { channel, messages })
        .await;
}

/// Load older locally persisted chat history rows before `before_ts_ms` and
/// replay them as `AppEvent::HistoryLoaded` for incremental backfill.
async fn load_local_older_messages(
    channel: ChannelId,
    before_ts_ms: i64,
    limit: usize,
    log_store: LogStore,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let channel_for_query = channel.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        log_store.older_messages(&channel_for_query, before_ts_ms, limit)
    })
    .await;

    let mut messages = match loaded {
        Ok(Ok(rows)) => rows,
        Ok(Err(e)) => {
            warn!(
                "chat-history: local older SQLite load failed for #{}: {e}",
                channel.display_name()
            );
            return;
        }
        Err(e) => {
            warn!(
                "chat-history: local older SQLite task failed for #{}: {e}",
                channel.display_name()
            );
            return;
        }
    };

    if messages.is_empty() {
        return;
    }

    for msg in &mut messages {
        msg.id = MessageId(HISTORY_MSG_ID.fetch_sub(1, std::sync::atomic::Ordering::Relaxed));
        msg.flags.is_history = true;
        msg.channel = channel.clone();
    }

    info!(
        "chat-history: loaded {} older local SQLite messages for #{}",
        messages.len(),
        channel.display_name()
    );
    let _ = evt_tx
        .send(AppEvent::HistoryLoaded { channel, messages })
        .await;
}

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

    info!("chat-history: fetching recent messages for #{ch}…");

    // NOTE: the correct path is /recent-messages/ (hyphen), not /recent_messages/.
    let robotty_url =
        format!("https://recent-messages.robotty.de/api/v2/recent-messages/{ch}?limit=800");
    let ivr_url = format!("https://logs.ivr.fi/channel/{ch}?json=1&reverse=true&limit=800");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    // Try robotty first; it covers all channels (including small ones).
    // Fall back to IVR if robotty fails or returns nothing.
    #[allow(unused_assignments)]
    let mut robotty_err: Option<String> = None;
    let raw_lines: Vec<String> = 'fetch: {
        match client
            .get(&robotty_url)
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    match resp.text().await {
                        Ok(text) => {
                            #[derive(serde::Deserialize)]
                            struct RobottyResponse {
                                messages: Vec<String>,
                            }
                            match serde_json::from_str::<RobottyResponse>(&text) {
                                Ok(p) if !p.messages.is_empty() => {
                                    info!(
                                        "chat-history: robotty returned {} raw lines for #{ch}",
                                        p.messages.len()
                                    );
                                    break 'fetch p.messages;
                                }
                                Ok(_) => {
                                    robotty_err = Some("robotty returned 0 messages".to_owned());
                                }
                                Err(e) => {
                                    robotty_err = Some(format!("robotty JSON parse failed: {e}"));
                                }
                            }
                        }
                        Err(e) => {
                            robotty_err = Some(format!("robotty body read failed: {e}"));
                        }
                    }
                } else {
                    robotty_err = Some(format!("robotty HTTP {status}"));
                }
            }
            Err(e) => {
                robotty_err = Some(format!("robotty request failed: {e}"));
            }
        }

        if let Some(ref err) = robotty_err {
            info!("chat-history: {err}, trying IVR fallback for #{ch}");
        }

        // IVR fallback
        match client
            .get(&ivr_url)
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.text().await {
                    Ok(text) => {
                        #[derive(serde::Deserialize)]
                        struct IvrMsg {
                            raw: String,
                        }
                        #[derive(serde::Deserialize)]
                        struct IvrResp {
                            messages: Vec<IvrMsg>,
                        }
                        match serde_json::from_str::<IvrResp>(&text) {
                            Ok(mut p) if !p.messages.is_empty() => {
                                p.messages.reverse(); // IVR is newest-first
                                info!(
                                    "chat-history: IVR returned {} raw lines for #{ch}",
                                    p.messages.len()
                                );
                                break 'fetch p.messages.into_iter().map(|m| m.raw).collect();
                            }
                            Ok(_) => {
                                warn!("chat-history: both sources returned 0 messages for #{ch}");
                            }
                            Err(e) => {
                                warn!("chat-history: IVR JSON parse failed for #{ch}: {e}");
                            }
                        }
                        Vec::new()
                    }
                    Err(e) => {
                        warn!("chat-history: IVR body read failed for #{ch}: {e}");
                        Vec::new()
                    }
                }
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
        info!("chat-history: loaded 0 historical messages for #{ch}");
        let _ = evt_tx
            .send(AppEvent::HistoryLoaded {
                channel: channel_id,
                messages: Vec::new(),
            })
            .await;
        return;
    }

    let raw_line_count = raw_lines.len();

    // ── Snapshot shared state once before the parse loop ────────────────────
    // Taking these locks inside the loop (800+ times) is expensive; snapshot
    // once and release immediately so other tasks aren't blocked.
    let emote_snapshot: HashMap<String, EmoteInfo> = {
        let guard = emote_index.read().unwrap();
        guard.clone()
    };
    let badge_snapshot: HashMap<(String, String, String), String> = {
        let bm = badge_map.read().unwrap();
        bm.clone()
    };
    let local_nick_owned = local_nick.map(str::to_owned);
    let channel_scope = ch.to_owned();

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
                    resolve_emote(&emote_snapshot, code).map(|info| {
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
                badge.url =
                    resolve_badge_url(&badge_snapshot, &channel_scope, &badge.name, &badge.version);
            }

            // Mention detection
            if let Some(ref nick) = local_nick_owned {
                let nick_lower = nick.to_lowercase();
                let text_lower = msg.raw_text.to_lowercase();
                // @mention or bare username as a whole word
                let has_mention = text_lower.contains(&format!("@{nick_lower}"))
                    || text_lower
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .any(|w| w == nick_lower);
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
        info!("chat-history: parsed 0 PRIVMSG lines from {raw_line_count} raw lines for #{ch}");
        let _ = evt_tx
            .send(AppEvent::HistoryLoaded {
                channel: channel_id,
                messages,
            })
            .await;
        return;
    }

    info!("chat-history: loaded {} messages for #{ch}", messages.len());

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
                .send(SevenTvCosmeticUpdate::BatchUserLookup {
                    user_ids: history_user_ids,
                })
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
        emit_helix_system_info(
            evt_tx,
            channel,
            format!("Moderation: {target_login} {verb}."),
        )
        .await;
        return;
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
        emit_helix_system_info(
            evt_tx,
            channel,
            format!("Moderation: {target_login} unbanned/untimed out."),
        )
        .await;
        return;
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

/// Update a channel-points redemption status via
/// `PATCH /helix/channel_points/custom_rewards/redemptions`.
async fn helix_update_reward_redemption_status(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    reward_id: &str,
    redemption_id: &str,
    status: &str,
    user_login: &str,
    reward_title: &str,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let (Some(cid), Some(bid)) = (client_id, broadcaster_id) else {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Channel Points".into(),
                message: "Cannot update redemption: missing Twitch credentials.".into(),
            })
            .await;
        return;
    };

    if reward_id.trim().is_empty() || redemption_id.trim().is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Channel Points".into(),
                message: "Cannot update redemption: missing reward/redemption identifiers.".into(),
            })
            .await;
        return;
    }

    let status_norm = status.trim().to_ascii_uppercase();
    if status_norm != "FULFILLED" && status_norm != "CANCELED" {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Channel Points".into(),
                message: format!("Unsupported redemption status: {status}"),
            })
            .await;
        return;
    }

    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    let url = format!(
        "https://api.twitch.tv/helix/channel_points/custom_rewards/redemptions\
         ?broadcaster_id={bid}&reward_id={reward_id}&id={redemption_id}"
    );

    #[derive(serde::Serialize)]
    struct RedemptionStatusBody<'a> {
        status: &'a str,
    }

    let client = reqwest::Client::new();
    let resp = match client
        .patch(&url)
        .header("Authorization", format!("Bearer {bare}"))
        .header("Client-Id", cid)
        .json(&RedemptionStatusBody {
            status: &status_norm,
        })
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Channel Points".into(),
                    message: format!("Redemption update request failed: {e}"),
                })
                .await;
            return;
        }
    };

    let http_status = resp.status();
    if http_status.is_success() {
        let action = if status_norm == "FULFILLED" {
            "approved"
        } else {
            "rejected"
        };
        emit_helix_system_info(
            evt_tx,
            channel,
            format!("{action} redemption '{reward_title}' from {user_login}."),
        )
        .await;
        return;
    }

    let body = resp.text().await.unwrap_or_default();
    let helix_msg = helix_error_message(http_status, &body);
    let _ = evt_tx
        .send(AppEvent::Error {
            context: "Channel Points".into(),
            message: format!("Could not update redemption: {helix_msg}"),
        })
        .await;
}

async fn emit_helix_system_info(evt_tx: mpsc::Sender<AppEvent>, channel: &ChannelId, text: String) {
    let msg = make_system_message(
        HISTORY_MSG_ID.fetch_sub(1, std::sync::atomic::Ordering::Relaxed),
        channel.clone(),
        text,
        Utc::now(),
        MsgKind::SystemInfo,
    );
    let _ = evt_tx
        .send(AppEvent::MessageReceived {
            channel: channel.clone(),
            message: msg,
        })
        .await;
}

fn helix_error_message(status: reqwest::StatusCode, body_text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body_text)
        .ok()
        .and_then(|v| {
            v.get("message")
                .and_then(|m| m.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| format!("HTTP {status}"))
}

fn require_helix_context<'a>(
    client_id: Option<&'a str>,
    broadcaster_id: Option<&'a str>,
) -> Result<(&'a str, &'a str), String> {
    let cid = client_id.ok_or_else(|| "Missing Twitch Client-ID.".to_owned())?;
    let bid = broadcaster_id.ok_or_else(|| {
        "Channel metadata not ready yet. Wait a moment, then retry command.".to_owned()
    })?;
    Ok((cid, bid))
}

fn require_helix_moderation_context<'a>(
    client_id: Option<&'a str>,
    broadcaster_id: Option<&'a str>,
    moderator_id: Option<&'a str>,
) -> Result<(&'a str, &'a str, &'a str), String> {
    let (cid, bid) = require_helix_context(client_id, broadcaster_id)?;
    let mid = moderator_id.ok_or_else(|| {
        "Missing moderator identity. Reconnect and try again once chat auth finishes.".to_owned()
    })?;
    Ok((cid, bid, mid))
}

async fn helix_user_id_by_login(
    bare_token: &str,
    client_id: &str,
    login: &str,
) -> Result<String, String> {
    #[derive(serde::Deserialize)]
    struct UserItem {
        id: String,
    }
    #[derive(serde::Deserialize)]
    struct UsersResponse {
        data: Vec<UserItem>,
    }

    let url = format!("https://api.twitch.tv/helix/users?login={login}");
    let client = reqwest::Client::new();
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {bare_token}"))
        .header("Client-Id", client_id)
        .send()
        .await
        .map_err(|e| format!("Lookup user '{login}' failed: {e}"))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "Lookup user '{login}' failed: {}",
            helix_error_message(status, &body)
        ));
    }

    let parsed = serde_json::from_str::<UsersResponse>(&body)
        .map_err(|e| format!("Failed to parse user lookup response: {e}"))?;
    let user = parsed
        .data
        .into_iter()
        .next()
        .ok_or_else(|| format!("Could not find Twitch channel '{login}'."))?;
    Ok(user.id)
}

async fn helix_send_announcement(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    moderator_id: Option<&str>,
    message: &str,
    color: Option<&str>,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let (cid, bid, mid) =
        match require_helix_moderation_context(client_id, broadcaster_id, moderator_id) {
            Ok(v) => v,
            Err(msg) => {
                let _ = evt_tx
                    .send(AppEvent::Error {
                        context: "Announcement".into(),
                        message: msg,
                    })
                    .await;
                return;
            }
        };

    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    if bare.trim().is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Announcement".into(),
                message: "You must be logged in to send announcements.".into(),
            })
            .await;
        return;
    }

    let trimmed = message.trim();
    if trimmed.is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Announcement".into(),
                message: "Announcement message cannot be empty.".into(),
            })
            .await;
        return;
    }
    if trimmed.chars().count() > 500 {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Announcement".into(),
                message: "Announcement message must be 500 characters or fewer.".into(),
            })
            .await;
        return;
    }

    let normalized_color = color
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_ascii_lowercase)
        .filter(|c| matches!(c.as_str(), "primary" | "blue" | "green" | "orange" | "purple"));

    #[derive(serde::Serialize)]
    struct AnnouncementBody<'a> {
        message: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        color: Option<&'a str>,
    }

    let url = format!(
        "https://api.twitch.tv/helix/chat/announcements?broadcaster_id={bid}&moderator_id={mid}"
    );
    let req = AnnouncementBody {
        message: trimmed,
        color: normalized_color.as_deref(),
    };

    let client = reqwest::Client::new();
    let resp = match client
        .post(url)
        .header("Authorization", format!("Bearer {bare}"))
        .header("Client-Id", cid)
        .json(&req)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Announcement".into(),
                    message: format!("Send announcement request failed: {e}"),
                })
                .await;
            return;
        }
    };

    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        let color_note = normalized_color
            .as_deref()
            .map(|c| format!(" ({c})"))
            .unwrap_or_default();
        emit_helix_system_info(
            evt_tx,
            channel,
            format!("Sent announcement{color_note}: {trimmed}"),
        )
        .await;
        return;
    }

    let msg = helix_error_message(status, &body_text);
    let _ = evt_tx
        .send(AppEvent::Error {
            context: "Announcement".into(),
            message: format!("Could not send announcement: {msg}"),
        })
        .await;
}

async fn helix_send_shoutout(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    moderator_id: Option<&str>,
    target_login: &str,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let (cid, bid, mid) =
        match require_helix_moderation_context(client_id, broadcaster_id, moderator_id) {
            Ok(v) => v,
            Err(msg) => {
                let _ = evt_tx
                    .send(AppEvent::Error {
                        context: "Shoutout".into(),
                        message: msg,
                    })
                    .await;
                return;
            }
        };

    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    if bare.trim().is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Shoutout".into(),
                message: "You must be logged in to send shoutouts.".into(),
            })
            .await;
        return;
    }

    let login = target_login.trim().trim_start_matches('@').to_ascii_lowercase();
    if login.is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Shoutout".into(),
                message: "Usage: /shoutout <channel>".into(),
            })
            .await;
        return;
    }
    if login.eq_ignore_ascii_case(channel.display_name()) {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Shoutout".into(),
                message: "You cannot shout out the current channel.".into(),
            })
            .await;
        return;
    }

    let to_broadcaster_id = match helix_user_id_by_login(bare, cid, &login).await {
        Ok(id) => id,
        Err(msg) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Shoutout".into(),
                    message: msg,
                })
                .await;
            return;
        }
    };

    let url = format!(
        "https://api.twitch.tv/helix/chat/shoutouts?from_broadcaster_id={bid}&to_broadcaster_id={to_broadcaster_id}&moderator_id={mid}"
    );

    let client = reqwest::Client::new();
    let resp = match client
        .post(url)
        .header("Authorization", format!("Bearer {bare}"))
        .header("Client-Id", cid)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Shoutout".into(),
                    message: format!("Send shoutout request failed: {e}"),
                })
                .await;
            return;
        }
    };

    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        emit_helix_system_info(evt_tx, channel, format!("Sent shoutout to {login}."))
            .await;
        return;
    }

    let msg = helix_error_message(status, &body_text);
    let _ = evt_tx
        .send(AppEvent::Error {
            context: "Shoutout".into(),
            message: format!("Could not send shoutout: {msg}"),
        })
        .await;
}

async fn helix_start_commercial(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    length_secs: u32,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let (cid, bid) = match require_helix_context(client_id, broadcaster_id) {
        Ok(v) => v,
        Err(msg) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Commercial".into(),
                    message: msg,
                })
                .await;
            return;
        }
    };

    if !matches!(length_secs, 30 | 60 | 90 | 120 | 150 | 180) {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Commercial".into(),
                message: "Commercial length must be one of 30, 60, 90, 120, 150, or 180 seconds."
                    .into(),
            })
            .await;
        return;
    }

    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    if bare.trim().is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Commercial".into(),
                message: "You must be logged in to start a commercial.".into(),
            })
            .await;
        return;
    }

    #[derive(serde::Serialize)]
    struct CommercialBody<'a> {
        broadcaster_id: &'a str,
        length: u32,
    }

    #[derive(serde::Deserialize)]
    struct CommercialData {
        length: Option<u32>,
        message: Option<String>,
        retry_after: Option<u32>,
    }

    #[derive(serde::Deserialize)]
    struct CommercialResponse {
        data: Vec<CommercialData>,
    }

    let req = CommercialBody {
        broadcaster_id: bid,
        length: length_secs,
    };

    let client = reqwest::Client::new();
    let resp = match client
        .post("https://api.twitch.tv/helix/channels/commercial")
        .header("Authorization", format!("Bearer {bare}"))
        .header("Client-Id", cid)
        .json(&req)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Commercial".into(),
                    message: format!("Start commercial request failed: {e}"),
                })
                .await;
            return;
        }
    };

    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        let info = serde_json::from_str::<CommercialResponse>(&body_text)
            .ok()
            .and_then(|payload| payload.data.into_iter().next());
        let confirmed_len = info
            .as_ref()
            .and_then(|d| d.length)
            .unwrap_or(length_secs);
        let server_msg = info
            .as_ref()
            .and_then(|d| d.message.as_deref())
            .filter(|m| !m.trim().is_empty())
            .unwrap_or("Commercial started.");
        let retry_hint = info
            .as_ref()
            .and_then(|d| d.retry_after)
            .map(|retry| format!(" Next one available in {retry}s."))
            .unwrap_or_default();
        emit_helix_system_info(
            evt_tx,
            channel,
            format!("Started {confirmed_len}s commercial. {server_msg}{retry_hint}"),
        )
        .await;
        return;
    }

    let msg = helix_error_message(status, &body_text);
    let _ = evt_tx
        .send(AppEvent::Error {
            context: "Commercial".into(),
            message: format!("Could not start commercial: {msg}"),
        })
        .await;
}

async fn helix_create_stream_marker(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    description: Option<&str>,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let (cid, bid) = match require_helix_context(client_id, broadcaster_id) {
        Ok(v) => v,
        Err(msg) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Marker".into(),
                    message: msg,
                })
                .await;
            return;
        }
    };

    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    if bare.trim().is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Marker".into(),
                message: "You must be logged in to create a stream marker.".into(),
            })
            .await;
        return;
    }

    let description = description.map(str::trim).filter(|d| !d.is_empty());
    if let Some(desc) = description {
        if desc.chars().count() > 140 {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Marker".into(),
                    message: "Marker description must be 140 characters or fewer.".into(),
                })
                .await;
            return;
        }
    }

    #[derive(serde::Serialize)]
    struct MarkerBody<'a> {
        user_id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<&'a str>,
    }

    #[derive(serde::Deserialize)]
    struct MarkerData {
        id: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct MarkerResponse {
        data: Vec<MarkerData>,
    }

    let req = MarkerBody {
        user_id: bid,
        description,
    };

    let client = reqwest::Client::new();
    let resp = match client
        .post("https://api.twitch.tv/helix/streams/markers")
        .header("Authorization", format!("Bearer {bare}"))
        .header("Client-Id", cid)
        .json(&req)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Marker".into(),
                    message: format!("Create marker request failed: {e}"),
                })
                .await;
            return;
        }
    };

    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        let marker_id = serde_json::from_str::<MarkerResponse>(&body_text)
            .ok()
            .and_then(|payload| payload.data.into_iter().next())
            .and_then(|entry| entry.id);
        let mut summary = if let Some(desc) = description {
            format!("Created stream marker: {desc}")
        } else {
            "Created stream marker.".to_owned()
        };
        if let Some(id) = marker_id {
            summary.push_str(&format!(" (id: {id})"));
        }
        emit_helix_system_info(evt_tx, channel, summary).await;
        return;
    }

    let msg = helix_error_message(status, &body_text);
    let _ = evt_tx
        .send(AppEvent::Error {
            context: "Marker".into(),
            message: format!("Could not create stream marker: {msg}"),
        })
        .await;
}

async fn helix_create_poll(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    title: &str,
    choices: &[String],
    duration_secs: u32,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let (cid, bid) = match require_helix_context(client_id, broadcaster_id) {
        Ok(v) => v,
        Err(msg) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Poll".into(),
                    message: msg,
                })
                .await;
            return;
        }
    };

    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    if bare.trim().is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Poll".into(),
                message: "You must be logged in to manage polls.".into(),
            })
            .await;
        return;
    }

    #[derive(serde::Serialize)]
    struct PollChoice<'a> {
        title: &'a str,
    }

    #[derive(serde::Serialize)]
    struct PollBody<'a> {
        broadcaster_id: &'a str,
        title: &'a str,
        choices: Vec<PollChoice<'a>>,
        duration: u32,
    }

    let req = PollBody {
        broadcaster_id: bid,
        title,
        choices: choices.iter().map(|c| PollChoice { title: c }).collect(),
        duration: duration_secs,
    };

    let client = reqwest::Client::new();
    let resp = match client
        .post("https://api.twitch.tv/helix/polls")
        .header("Authorization", format!("Bearer {bare}"))
        .header("Client-Id", cid)
        .json(&req)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Poll".into(),
                    message: format!("Create poll request failed: {e}"),
                })
                .await;
            return;
        }
    };

    let status = resp.status();
    if status.is_success() {
        emit_helix_system_info(
            evt_tx,
            channel,
            format!(
                "Started poll: {title} ({} choices, {}s)",
                choices.len(),
                duration_secs
            ),
        )
        .await;
        return;
    }

    let body_text = resp.text().await.unwrap_or_default();
    let msg = helix_error_message(status, &body_text);
    let _ = evt_tx
        .send(AppEvent::Error {
            context: "Poll".into(),
            message: format!("Could not create poll: {msg}"),
        })
        .await;
}

async fn helix_find_active_poll(
    bare_token: &str,
    client_id: &str,
    broadcaster_id: &str,
) -> Result<(String, String), String> {
    #[derive(serde::Deserialize)]
    struct PollItem {
        id: String,
        title: String,
        status: String,
    }
    #[derive(serde::Deserialize)]
    struct PollList {
        data: Vec<PollItem>,
    }

    let url = format!(
        "https://api.twitch.tv/helix/polls?broadcaster_id={broadcaster_id}&first=20"
    );
    let client = reqwest::Client::new();
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {bare_token}"))
        .header("Client-Id", client_id)
        .send()
        .await
        .map_err(|e| format!("Fetch active poll failed: {e}"))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "Fetch active poll failed: {}",
            helix_error_message(status, &body)
        ));
    }

    let parsed = serde_json::from_str::<PollList>(&body)
        .map_err(|e| format!("Failed to parse poll list: {e}"))?;
    let active = parsed
        .data
        .into_iter()
        .find(|p| p.status.eq_ignore_ascii_case("ACTIVE"))
        .ok_or_else(|| "No active poll found in this channel.".to_owned())?;
    Ok((active.id, active.title))
}

async fn helix_end_poll(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    status: &str,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let (cid, bid) = match require_helix_context(client_id, broadcaster_id) {
        Ok(v) => v,
        Err(msg) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Poll".into(),
                    message: msg,
                })
                .await;
            return;
        }
    };

    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    if bare.trim().is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Poll".into(),
                message: "You must be logged in to manage polls.".into(),
            })
            .await;
        return;
    }

    let (poll_id, poll_title) = match helix_find_active_poll(bare, cid, bid).await {
        Ok(v) => v,
        Err(msg) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Poll".into(),
                    message: msg,
                })
                .await;
            return;
        }
    };

    #[derive(serde::Serialize)]
    struct EndPollBody<'a> {
        broadcaster_id: &'a str,
        id: &'a str,
        status: &'a str,
    }

    let req = EndPollBody {
        broadcaster_id: bid,
        id: &poll_id,
        status,
    };

    let client = reqwest::Client::new();
    let resp = match client
        .patch("https://api.twitch.tv/helix/polls")
        .header("Authorization", format!("Bearer {bare}"))
        .header("Client-Id", cid)
        .json(&req)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Poll".into(),
                    message: format!("End poll request failed: {e}"),
                })
                .await;
            return;
        }
    };

    let http_status = resp.status();
    if http_status.is_success() {
        let verb = if status.eq_ignore_ascii_case("TERMINATED") {
            "Canceled"
        } else {
            "Ended"
        };
        emit_helix_system_info(evt_tx, channel, format!("{verb} poll: {poll_title}"))
            .await;
        return;
    }

    let body_text = resp.text().await.unwrap_or_default();
    let msg = helix_error_message(http_status, &body_text);
    let _ = evt_tx
        .send(AppEvent::Error {
            context: "Poll".into(),
            message: format!("Could not update poll: {msg}"),
        })
        .await;
}

#[derive(Debug, Clone)]
struct HelixPredictionOutcome {
    id: String,
    title: String,
}

#[derive(Debug, Clone)]
struct HelixPredictionState {
    id: String,
    title: String,
    status: String,
    outcomes: Vec<HelixPredictionOutcome>,
}

async fn helix_find_manageable_prediction(
    bare_token: &str,
    client_id: &str,
    broadcaster_id: &str,
) -> Result<HelixPredictionState, String> {
    #[derive(serde::Deserialize)]
    struct OutcomeItem {
        id: String,
        title: String,
    }
    #[derive(serde::Deserialize)]
    struct PredictionItem {
        id: String,
        title: String,
        status: String,
        outcomes: Vec<OutcomeItem>,
    }
    #[derive(serde::Deserialize)]
    struct PredictionList {
        data: Vec<PredictionItem>,
    }

    let url = format!(
        "https://api.twitch.tv/helix/predictions?broadcaster_id={broadcaster_id}&first=20"
    );
    let client = reqwest::Client::new();
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {bare_token}"))
        .header("Client-Id", client_id)
        .send()
        .await
        .map_err(|e| format!("Fetch active prediction failed: {e}"))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "Fetch active prediction failed: {}",
            helix_error_message(status, &body)
        ));
    }

    let parsed = serde_json::from_str::<PredictionList>(&body)
        .map_err(|e| format!("Failed to parse prediction list: {e}"))?;
    let selected = parsed
        .data
        .into_iter()
        .find(|p| {
            p.status.eq_ignore_ascii_case("LOCKED") || p.status.eq_ignore_ascii_case("ACTIVE")
        })
        .ok_or_else(|| "No active or locked prediction found in this channel.".to_owned())?;

    Ok(HelixPredictionState {
        id: selected.id,
        title: selected.title,
        status: selected.status,
        outcomes: selected
            .outcomes
            .into_iter()
            .map(|o| HelixPredictionOutcome {
                id: o.id,
                title: o.title,
            })
            .collect(),
    })
}

async fn helix_create_prediction(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    title: &str,
    outcomes: &[String],
    duration_secs: u32,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let (cid, bid) = match require_helix_context(client_id, broadcaster_id) {
        Ok(v) => v,
        Err(msg) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Prediction".into(),
                    message: msg,
                })
                .await;
            return;
        }
    };

    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    if bare.trim().is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Prediction".into(),
                message: "You must be logged in to manage predictions.".into(),
            })
            .await;
        return;
    }

    #[derive(serde::Serialize)]
    struct PredictionOutcome<'a> {
        title: &'a str,
    }

    #[derive(serde::Serialize)]
    struct PredictionBody<'a> {
        broadcaster_id: &'a str,
        title: &'a str,
        outcomes: Vec<PredictionOutcome<'a>>,
        prediction_window: u32,
    }

    let req = PredictionBody {
        broadcaster_id: bid,
        title,
        outcomes: outcomes
            .iter()
            .map(|outcome| PredictionOutcome { title: outcome })
            .collect(),
        prediction_window: duration_secs,
    };

    let client = reqwest::Client::new();
    let resp = match client
        .post("https://api.twitch.tv/helix/predictions")
        .header("Authorization", format!("Bearer {bare}"))
        .header("Client-Id", cid)
        .json(&req)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Prediction".into(),
                    message: format!("Create prediction request failed: {e}"),
                })
                .await;
            return;
        }
    };

    let status = resp.status();
    if status.is_success() {
        emit_helix_system_info(
            evt_tx,
            channel,
            format!(
                "Started prediction: {title} ({} outcomes, {}s)",
                outcomes.len(),
                duration_secs
            ),
        )
        .await;
        return;
    }

    let body_text = resp.text().await.unwrap_or_default();
    let msg = helix_error_message(status, &body_text);
    let _ = evt_tx
        .send(AppEvent::Error {
            context: "Prediction".into(),
            message: format!("Could not create prediction: {msg}"),
        })
        .await;
}

async fn helix_patch_prediction_status(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
    desired_status: &str,
    winning_outcome_index: Option<usize>,
) {
    let (cid, bid) = match require_helix_context(client_id, broadcaster_id) {
        Ok(v) => v,
        Err(msg) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Prediction".into(),
                    message: msg,
                })
                .await;
            return;
        }
    };

    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    if bare.trim().is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Prediction".into(),
                message: "You must be logged in to manage predictions.".into(),
            })
            .await;
        return;
    }

    let prediction = match helix_find_manageable_prediction(bare, cid, bid).await {
        Ok(v) => v,
        Err(msg) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Prediction".into(),
                    message: msg,
                })
                .await;
            return;
        }
    };

    if desired_status.eq_ignore_ascii_case("LOCKED")
        && !prediction.status.eq_ignore_ascii_case("ACTIVE")
    {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Prediction".into(),
                message: format!(
                    "Prediction is {} and cannot be locked.",
                    prediction.status
                ),
            })
            .await;
        return;
    }

    let winning_outcome_id = if let Some(index_1based) = winning_outcome_index {
        let index = index_1based.saturating_sub(1);
        let Some(outcome) = prediction.outcomes.get(index) else {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Prediction".into(),
                    message: format!(
                        "Outcome index {} is out of range. Available outcomes: {}.",
                        index_1based,
                        prediction.outcomes.len()
                    ),
                })
                .await;
            return;
        };
        Some((outcome.id.clone(), outcome.title.clone()))
    } else {
        None
    };

    #[derive(serde::Serialize)]
    struct PatchPredictionBody<'a> {
        broadcaster_id: &'a str,
        id: &'a str,
        status: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        winning_outcome_id: Option<&'a str>,
    }

    let req = PatchPredictionBody {
        broadcaster_id: bid,
        id: &prediction.id,
        status: desired_status,
        winning_outcome_id: winning_outcome_id.as_ref().map(|(id, _)| id.as_str()),
    };

    let client = reqwest::Client::new();
    let resp = match client
        .patch("https://api.twitch.tv/helix/predictions")
        .header("Authorization", format!("Bearer {bare}"))
        .header("Client-Id", cid)
        .json(&req)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Prediction".into(),
                    message: format!("Update prediction request failed: {e}"),
                })
                .await;
            return;
        }
    };

    let http_status = resp.status();
    if http_status.is_success() {
        let summary = if desired_status.eq_ignore_ascii_case("LOCKED") {
            format!("Locked prediction: {}", prediction.title)
        } else if desired_status.eq_ignore_ascii_case("CANCELED") {
            format!("Canceled prediction: {}", prediction.title)
        } else if desired_status.eq_ignore_ascii_case("RESOLVED") {
            let winner = winning_outcome_id
                .as_ref()
                .map(|(_, title)| title.as_str())
                .unwrap_or("<unknown>");
            format!("Resolved prediction: {} (winner: {})", prediction.title, winner)
        } else {
            format!("Updated prediction: {}", prediction.title)
        };
        emit_helix_system_info(evt_tx, channel, summary).await;
        return;
    }

    let body_text = resp.text().await.unwrap_or_default();
    let msg = helix_error_message(http_status, &body_text);
    let _ = evt_tx
        .send(AppEvent::Error {
            context: "Prediction".into(),
            message: format!("Could not update prediction: {msg}"),
        })
        .await;
}

async fn helix_lock_prediction(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    helix_patch_prediction_status(
        token,
        client_id,
        broadcaster_id,
        channel,
        evt_tx,
        "LOCKED",
        None,
    )
    .await;
}

async fn helix_resolve_prediction(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    winning_outcome_index: usize,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    helix_patch_prediction_status(
        token,
        client_id,
        broadcaster_id,
        channel,
        evt_tx,
        "RESOLVED",
        Some(winning_outcome_index),
    )
    .await;
}

async fn helix_cancel_prediction(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    helix_patch_prediction_status(
        token,
        client_id,
        broadcaster_id,
        channel,
        evt_tx,
        "CANCELED",
        None,
    )
    .await;
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

#[derive(Debug, Clone)]
struct TwitchPinnedSnapshot {
    pinned_id: String,
    sender_id: String,
    sender_login: String,
    sender_display_name: String,
    text: String,
    sent_at: Option<String>,
    starts_at: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct TwitchPinnedQueryResponse {
    data: Option<TwitchPinnedQueryData>,
}

#[derive(Debug, serde::Deserialize)]
struct TwitchPinnedQueryData {
    user: Option<TwitchPinnedUser>,
}

#[derive(Debug, serde::Deserialize)]
struct TwitchPinnedUser {
    channel: Option<TwitchPinnedChannel>,
}

#[derive(Debug, serde::Deserialize)]
struct TwitchPinnedChannel {
    #[serde(rename = "pinnedChatMessages")]
    pinned_chat_messages: Option<TwitchPinnedConnection>,
}

#[derive(Debug, serde::Deserialize)]
struct TwitchPinnedConnection {
    edges: Vec<TwitchPinnedEdge>,
}

#[derive(Debug, serde::Deserialize)]
struct TwitchPinnedEdge {
    node: TwitchPinnedNode,
}

#[derive(Debug, serde::Deserialize)]
struct TwitchPinnedNode {
    id: String,
    #[serde(rename = "startsAt")]
    starts_at: Option<String>,
    #[serde(rename = "pinnedMessage")]
    pinned_message: TwitchPinnedMessage,
}

#[derive(Debug, serde::Deserialize)]
struct TwitchPinnedMessage {
    content: TwitchPinnedContent,
    sender: TwitchPinnedSender,
    #[serde(rename = "sentAt")]
    sent_at: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct TwitchPinnedContent {
    text: String,
}

#[derive(Debug, serde::Deserialize)]
struct TwitchPinnedSender {
    id: String,
    login: String,
    #[serde(rename = "displayName")]
    display_name: String,
}

fn parse_twitch_pinned_snapshot_json(body: &str) -> Option<TwitchPinnedSnapshot> {
    let parsed: TwitchPinnedQueryResponse = serde_json::from_str(body).ok()?;
    let edge = parsed
        .data?
        .user?
        .channel?
        .pinned_chat_messages?
        .edges
        .into_iter()
        .next()?;
    let node = edge.node;
    let text = node.pinned_message.content.text.trim().to_owned();
    if text.is_empty() {
        return None;
    }
    Some(TwitchPinnedSnapshot {
        pinned_id: node.id,
        sender_id: node.pinned_message.sender.id,
        sender_login: node.pinned_message.sender.login,
        sender_display_name: node.pinned_message.sender.display_name,
        text,
        sent_at: node.pinned_message.sent_at,
        starts_at: node.starts_at,
    })
}

async fn fetch_current_twitch_pinned_message(channel: ChannelId, evt_tx: mpsc::Sender<AppEvent>) {
    let channel_login = channel.as_str().trim().to_ascii_lowercase();
    if channel_login.is_empty() {
        return;
    }

    let payload = serde_json::json!({
        "query": "query($login:String!){user(login:$login){channel{id pinnedChatMessages(first:1){edges{node{id startsAt pinnedMessage{content{text} sender{id displayName login} sentAt}}}}}}}",
        "variables": { "login": channel_login },
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(6))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let resp = match client
        .post(TWITCH_GQL_URL)
        .header("Client-ID", TWITCH_WEB_CLIENT_ID)
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "pinned-fetch: request failed for #{}: {}",
                channel.display_name(),
                e
            );
            return;
        }
    };

    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            warn!(
                "pinned-fetch: failed reading response for #{}: {}",
                channel.display_name(),
                e
            );
            return;
        }
    };

    let Some(snapshot) = parse_twitch_pinned_snapshot_json(&body) else {
        return;
    };

    let timestamp = snapshot
        .sent_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|| {
            snapshot
                .starts_at
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc))
        })
        .unwrap_or_else(Utc::now);

    let mut msg = make_system_message(
        HISTORY_MSG_ID.fetch_sub(1, std::sync::atomic::Ordering::Relaxed),
        channel.clone(),
        snapshot.text,
        timestamp,
        MsgKind::SystemInfo,
    );
    msg.server_id = Some(format!("twitch:pinned:{}", snapshot.pinned_id));
    msg.sender = Sender {
        user_id: UserId(snapshot.sender_id),
        login: snapshot.sender_login,
        display_name: snapshot.sender_display_name,
        color: None,
        name_paint: None,
        badges: Vec::new(),
    };
    msg.flags.is_pinned = true;
    // Snapshot fetched on join; treat as historical context, not a fresh live event.
    msg.flags.is_history = true;

    let _ = evt_tx
        .send(AppEvent::MessageReceived {
            channel,
            message: msg,
        })
        .await;
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

#[cfg(test)]
mod tests {
    use super::{parse_twitch_pinned_snapshot_json, APP_INITIAL_INNER_SIZE, APP_MIN_INNER_SIZE};
    use crate::runtime::system_messages::is_twitch_pinned_notice;

    #[test]
    fn twitch_pinned_notice_detects_multi_line_pin_card_text() {
        let txt = "Pinned by ModeratorFieryVamp\n\nDon't forget to vote for your favorite content to be showcased here https://asmongold247.tv/\n\nModerator6-Month SubscriberDittoFieryVamp sent at 05:28 AM";
        assert!(is_twitch_pinned_notice(txt));
    }

    #[test]
    fn twitch_pinned_notice_ignores_regular_system_messages() {
        assert!(!is_twitch_pinned_notice("Joined channel"));
        assert!(!is_twitch_pinned_notice(
            "You are permanently banned from talking in this channel."
        ));
        assert!(!is_twitch_pinned_notice("The pinned message was unpinned."));
    }

    #[test]
    fn parse_twitch_pinned_snapshot_extracts_first_edge() {
        let json = r#"{
            "data": {
                "user": {
                    "channel": {
                        "pinnedChatMessages": {
                            "edges": [
                                {
                                    "node": {
                                        "id": "fa15fd37-56df-4349-98eb-412b0544d475",
                                        "startsAt": "2026-03-28T09:28:42Z",
                                        "pinnedMessage": {
                                            "content": {
                                                "text": "Don't forget to vote for your favorite content to be showcased here https://asmongold247.tv/"
                                            },
                                            "sender": {
                                                "id": "222687958",
                                                "displayName": "FieryVamp",
                                                "login": "fieryvamp"
                                            },
                                            "sentAt": "2026-03-28T09:28:39.861714831Z"
                                        }
                                    }
                                }
                            ]
                        }
                    }
                }
            }
        }"#;

        let parsed = parse_twitch_pinned_snapshot_json(json).expect("expected pinned snapshot");
        assert_eq!(parsed.pinned_id, "fa15fd37-56df-4349-98eb-412b0544d475");
        assert_eq!(parsed.sender_display_name, "FieryVamp");
        assert_eq!(
            parsed.text,
            "Don't forget to vote for your favorite content to be showcased here https://asmongold247.tv/"
        );
    }

    #[test]
    fn parse_twitch_pinned_snapshot_returns_none_when_no_edges() {
        let json = r#"{
            "data": {
                "user": {
                    "channel": {
                        "pinnedChatMessages": {
                            "edges": []
                        }
                    }
                }
            }
        }"#;

        assert!(parse_twitch_pinned_snapshot_json(json).is_none());
    }

    #[test]
    fn app_min_inner_size_allows_thinner_windows() {
        assert!(APP_MIN_INNER_SIZE[0] < 300.0);
        assert_eq!(APP_MIN_INNER_SIZE[1], 200.0);
        assert!(APP_INITIAL_INNER_SIZE[0] > APP_MIN_INNER_SIZE[0]);
    }
}

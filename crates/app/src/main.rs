use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use chrono::Utc;
use crust_core::events::{
    AppCommand, AppEvent, AutoModQueueItem, ConnectionState, UnbanRequestItem,
};
use crust_core::model::{
    Badge, ChannelId, ChatMessage, MessageFlags, MessageId, MsgKind, Sender, TwitchEmotePos, UserId,
};
use crust_core::plugins::{plugin_host, set_plugin_host, PluginCommandInvocation};
use crust_emotes::{cache::EmoteCache, providers::EmoteInfo};
use crust_kick::session::{KickEvent, KickSession, KickSessionCommand};
use crust_storage::{AppSettings, LogStore, SettingsStore};
use crust_twitch::session::generic_irc::{
    is_raw_irc_protocol_line, GenericIrcEvent, GenericIrcSession, GenericIrcSessionCommand,
};
use crust_twitch::{
    eventsub::{EventSubCommand, EventSubEvent, EventSubNoticeKind, EventSubSession},
    session::client::{SessionCommand, TwitchEvent, TwitchSession},
};
use crust_ui::CrustApp;
use seventv::{
    apply_7tv_cosmetics_to_sender, load_7tv_cosmetics_catalog, load_7tv_user_style_for_twitch,
    resolve_7tv_user_style, SevenTvBadgeMeta, SevenTvCosmeticUpdate, SevenTvPaintMeta,
    SevenTvResolvedStyle, SevenTvUserStyleRaw,
};

use runtime::assets::fetch_emote_image;
use runtime::badges::{
    load_badge_map_cache_into, load_channel_badges, load_global_badges, resolve_badge_url, BadgeMap,
};
use runtime::emote_loading::{
    load_channel_emotes, load_global_emotes, load_kick_channel_emotes, load_personal_7tv_emotes,
};
#[cfg(test)]
use runtime::eventsub_notices::format_eventsub_notice_text;
use runtime::eventsub_notices::{
    eventsub_notice_to_message, moderation_action_effect_from_notice,
    room_state_update_from_moderation_action, should_drop_duplicate_eventsub_notice,
    should_emit_eventsub_notice_message, stream_status_is_live_from_notice, ModerationActionEffect,
};
use runtime::history::{
    load_local_older_messages, load_local_recent_messages, load_local_recent_whispers,
    load_recent_messages,
};
use runtime::link_preview::fetch_link_preview;
use runtime::plugins::init_plugins;
use runtime::profiles::{
    fetch_ivr_logs, fetch_self_avatar, fetch_twitch_stream_status, fetch_twitch_user_profile,
    fetch_user_profile_for_channel,
};
use runtime::system_messages::{
    build_sub_text, extract_irc_msg_echo, format_timeout_text, is_twitch_pinned_notice,
    make_custom_message, make_system_message,
};

mod runtime;
mod seventv;

const CMD_CHANNEL_SIZE: usize = 512;
const EVT_CHANNEL_SIZE: usize = 4096;
const TWITCH_EVT_SIZE: usize = 4096;
const KICK_EVT_SIZE: usize = 4096;
const IRC_EVT_SIZE: usize = 4096;
const EVENTSUB_EVT_SIZE: usize = 1024;
const MODERATION_CMD_COOLDOWN: Duration = Duration::from_millis(500);
const TWITCH_MAX_MESSAGE_CHARS: usize = 500;
const TWITCH_GQL_URL: &str = "https://gql.twitch.tv/gql";
const TWITCH_WEB_CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const WHISPER_HISTORY_CHANNEL_PREFIX: &str = "whisper:";
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

/// Track which emote codes are global (vs channel-specific).
type GlobalCodes = Arc<RwLock<std::collections::HashSet<String>>>;

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
        info!(
            "Loaded {} badge mappings from local cache",
            loaded_cached_badges
        );
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

    // Lua/CupidScript plugin host.
    let _plugin_host = init_plugins(cmd_tx.clone(), initial_settings.use_24h_timestamps);
    let _ = set_plugin_host(_plugin_host.clone());

    // Apply persisted theme before UI renders.
    crust_ui::theme::apply_from_str(&initial_settings.theme);

    // Raise worker stack size before any tokio threads are created.
    // Some startup paths in OpenSSL/reqwest are deep enough to overflow the
    // default worker stack on this build when they run on background threads.
    if std::env::var_os("RUST_MIN_STACK").is_none() {
        std::env::set_var("RUST_MIN_STACK", "33554432");
    }

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
            if let Some(host) = plugin_host() {
                host.dispatch_event(&evt);
            }
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

    // Run the initial catalog bootstrap on the main thread instead of a
    // worker thread. The provider constructors and TLS setup involved here
    // can consume a lot of stack on some libc/OpenSSL combinations.
    {
        let idx = emote_index.clone();
        let cache = emote_cache.clone();
        let etx = evt_tx.clone();
        let gc = global_emote_codes.clone();
        let bm = badge_map.clone();
        let token = saved_token.clone();
        rt.block_on(async move {
            load_global_emotes(&idx, &cache, &etx, &gc).await;
            load_global_badges(&bm, &cache, &etx, token).await;
        });
    }

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
    Refresh {
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
    // Track recent EventSub metadata.message_id values to avoid applying
    // duplicated moderation/state events after websocket reconnects.
    let mut seen_eventsub_notice_ids: HashMap<String, Instant> = HashMap::new();
    let mut seen_eventsub_notice_gc_at = Instant::now();
    // Simple per-channel cooldown for local moderation actions to avoid
    // accidental duplicate requests from rapid clicks/hotkeys.
    let mut moderation_cmd_cooldowns: HashMap<String, Instant> = HashMap::new();

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
    let mut whisper_history_loaded_for: Option<String> = None;
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
            show_timestamp_seconds: settings.show_timestamp_seconds,
            use_24h_timestamps: settings.use_24h_timestamps,
            local_log_indexing_enabled: settings.local_log_indexing_enabled,
            auto_join: settings.auto_join.clone(),
            highlights: settings.highlights.clone(),
            ignores: settings.ignores.clone(),
            desktop_notifications_enabled: settings.desktop_notifications_enabled,
        })
        .await;
    let _ = evt_tx
        .send(AppEvent::SlashUsageCountsUpdated {
            usage_counts: settings
                .slash_usage_counts
                .iter()
                .map(|(name, count)| (name.clone(), *count))
                .collect(),
        })
        .await;
    let _ = evt_tx
        .send(AppEvent::EmotePickerPreferencesUpdated {
            favorites: settings.emote_picker_favorites.clone(),
            recent: settings.emote_picker_recent.clone(),
            provider_boost: settings.emote_picker_provider_boost.clone(),
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
                        if settings.local_log_indexing_enabled {
                            if let Some(store) = chat_logs.clone() {
                            let ch_local = channel.clone();
                            let etx_local = evt_tx.clone();
                            tokio::spawn(async move {
                                load_local_recent_messages(ch_local, store, etx_local).await;
                            });
                            }
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

                        if whisper_history_loaded_for
                            .as_deref()
                            != auth_username.as_deref()
                        {
                            whisper_history_loaded_for = auth_username.clone();
                            if settings.local_log_indexing_enabled {
                                if let Some(store) = chat_logs.clone() {
                                    let etx_whisper = evt_tx.clone();
                                    let self_login = auth_username.clone();
                                    tokio::spawn(async move {
                                        load_local_recent_whispers(store, self_login, etx_whisper)
                                            .await;
                                    });
                                }
                            }
                        }

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
                        if settings.local_log_indexing_enabled {
                            if let Some(store) = chat_logs.as_ref() {
                            if let Err(e) = store.append_message(&msg) {
                                warn!("chat-log: failed to persist Twitch message: {e}");
                            }
                            }
                        }
                        let _ = evt_tx.send(AppEvent::MessageReceived {
                            channel,
                            message: msg,
                        }).await;
                    }
                    TwitchEvent::Whisper {
                        from_login,
                        from_display_name,
                        target_login,
                        text,
                        twitch_emotes,
                        is_self,
                        timestamp,
                    } => {
                        if settings.local_log_indexing_enabled && !is_self {
                            if let Some(store) = chat_logs.as_ref() {
                                if let Err(e) = persist_whisper_message(
                                    store,
                                    auth_username.as_deref(),
                                    &from_login,
                                    &from_display_name,
                                    &target_login,
                                    &text,
                                    &twitch_emotes,
                                    is_self,
                                    timestamp,
                                ) {
                                    warn!("whisper-history: failed to persist whisper: {e}");
                                }
                            }
                        }
                        let _ = evt_tx
                            .send(AppEvent::WhisperReceived {
                                from_login,
                                from_display_name,
                                target_login,
                                text,
                                twitch_emotes,
                                is_self,
                                timestamp,
                                is_history: false,
                            })
                            .await;
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
                        let _ = evt_tx
                            .send(AppEvent::ChannelMessagesCleared {
                                channel: channel.clone(),
                            })
                            .await;
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
                        if settings.local_log_indexing_enabled {
                            if let Some(store) = chat_logs.as_ref() {
                            if let Err(e) = store.append_message(&msg) {
                                warn!("chat-log: failed to persist Kick message: {e}");
                            }
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
                        let _ = evt_tx
                            .send(AppEvent::ChannelMessagesCleared {
                                channel: channel.clone(),
                            })
                            .await;
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
                        if settings.local_log_indexing_enabled {
                            if let Some(store) = chat_logs.as_ref() {
                            if let Err(e) = store.append_message(&msg) {
                                warn!("chat-log: failed to persist IRC message: {e}");
                            }
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
                            let token = settings.oauth_token.clone();
                            let client_id = helix_client_id.clone();
                            tokio::spawn(async move {
                                fetch_twitch_user_profile(
                                    &login,
                                    Some(token.as_str()),
                                    client_id.as_deref(),
                                    etx,
                                )
                                .await;
                            });
                        }
                    }
                    EventSubEvent::Notice(notice) => {
                        if let Some(event_id) = notice.event_id.as_deref() {
                            let now = Instant::now();
                            if should_drop_duplicate_eventsub_notice(
                                &mut seen_eventsub_notice_ids,
                                event_id,
                                now,
                                &mut seen_eventsub_notice_gc_at,
                            ) {
                                debug!("Skipping duplicate EventSub notice id={event_id}");
                                continue;
                            }
                        }

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
                            let mut next_message_id = || {
                                let id = local_msg_id;
                                local_msg_id += 1;
                                id
                            };
                            let make_sender = |user_id: &str,
                                               login: &str,
                                               display_name: &str| Sender {
                                user_id: UserId(user_id.to_owned()),
                                login: login.to_owned(),
                                display_name: display_name.to_owned(),
                                color: None,
                                name_paint: None,
                                badges: Vec::new(),
                            };
                            let make_automod_sender = || Sender {
                                user_id: UserId(String::new()),
                                login: "automod".to_owned(),
                                display_name: "AutoMod".to_owned(),
                                color: None,
                                name_paint: None,
                                badges: vec![Badge {
                                    name: "twitchbot".to_owned(),
                                    version: "1".to_owned(),
                                    url: None,
                                }],
                            };
                            match &notice.kind {
                                EventSubNoticeKind::AutoModMessageHold {
                                    message_id,
                                    sender_user_id,
                                    sender_login,
                                    text,
                                    reason,
                                } => {
                                    if !message_id.trim().is_empty() {
                                        let _ = evt_tx
                                            .send(AppEvent::AutoModQueueAppend {
                                                channel: channel.clone(),
                                                item: AutoModQueueItem {
                                                    message_id: message_id.clone(),
                                                    sender_user_id: sender_user_id.clone(),
                                                    sender_login: sender_login.clone(),
                                                    text: text.clone(),
                                                    reason: reason.clone(),
                                                },
                                            })
                                            .await;
                                    }

                                    let msg = make_custom_message(
                                        next_message_id(),
                                        channel.clone(),
                                        "AutoMod: Held a message for review.".to_owned(),
                                        Utc::now(),
                                        make_automod_sender(),
                                        MessageFlags::default(),
                                        MsgKind::SystemInfo,
                                    );
                                    let _ = evt_tx
                                        .send(AppEvent::MessageReceived {
                                            channel: channel.clone(),
                                            message: msg,
                                        })
                                        .await;
                                }
                                EventSubNoticeKind::AutoModMessageUpdate {
                                    message_id,
                                    ..
                                } => {
                                    if !message_id.trim().is_empty() {
                                        let _ = evt_tx
                                            .send(AppEvent::AutoModQueueRemove {
                                                channel: channel.clone(),
                                                message_id: message_id.clone(),
                                                action: None,
                                            })
                                            .await;
                                    }
                                }
                                EventSubNoticeKind::ChannelChatUserMessageHold {
                                    message_id,
                                    user_id: _,
                                    user_login: _,
                                    user_name: _,
                                    ..
                                } => {
                                    if !message_id.trim().is_empty() {
                                    let msg = make_custom_message(
                                        next_message_id(),
                                        channel.clone(),
                                        "AutoMod: Hey! Your message is being checked by mods and has not been sent.".to_owned(),
                                        Utc::now(),
                                        make_automod_sender(),
                                        MessageFlags::default(),
                                        MsgKind::SystemInfo,
                                    );
                                        let _ = evt_tx
                                            .send(AppEvent::MessageReceived {
                                                channel: channel.clone(),
                                                message: msg,
                                            })
                                            .await;
                                    }
                                }
                                EventSubNoticeKind::ChannelChatUserMessageUpdate {
                                    message_id,
                                    user_id: _,
                                    user_login: _,
                                    user_name: _,
                                    status,
                                    ..
                                } => {
                                    if !message_id.trim().is_empty() {
                                        let message_text = match status.trim().to_ascii_lowercase().as_str()
                                        {
                                            "approved" => "AutoMod: Mods have accepted your message.",
                                            "denied" => "AutoMod: Mods have denied your message.",
                                            "invalid" => "AutoMod: Your message was lost in the void.",
                                            _ => "AutoMod: Message update resolved.",
                                        }
                                        .to_owned();
                                        let msg = make_custom_message(
                                            next_message_id(),
                                            channel.clone(),
                                            message_text,
                                            Utc::now(),
                                            make_automod_sender(),
                                            MessageFlags::default(),
                                            MsgKind::SystemInfo,
                                        );
                                        let _ = evt_tx
                                            .send(AppEvent::MessageReceived {
                                                channel: channel.clone(),
                                                message: msg,
                                            })
                                            .await;
                                    }
                                }
                                EventSubNoticeKind::UnbanRequestCreate {
                                    request_id,
                                    user_id,
                                    user_login,
                                    text,
                                    created_at,
                                } => {
                                    if !request_id.trim().is_empty() {
                                        let _ = evt_tx
                                            .send(AppEvent::UnbanRequestUpsert {
                                                channel: channel.clone(),
                                                request: UnbanRequestItem {
                                                    request_id: request_id.clone(),
                                                    user_id: user_id.clone(),
                                                    user_login: user_login.clone(),
                                                    text: text.clone(),
                                                    created_at: created_at.clone(),
                                                    status: Some("PENDING".to_owned()),
                                                },
                                            })
                                            .await;
                                    }
                                }
                                EventSubNoticeKind::UnbanRequestResolve {
                                    request_id,
                                    status,
                                } => {
                                    if !request_id.trim().is_empty() {
                                        let _ = evt_tx
                                            .send(AppEvent::UnbanRequestResolved {
                                                channel: channel.clone(),
                                                request_id: request_id.clone(),
                                                status: status.clone(),
                                            })
                                            .await;
                                    }
                                }
                                EventSubNoticeKind::ChannelBan { user_login, .. } => {
                                    if !user_login.trim().is_empty() {
                                        let _ = evt_tx
                                            .send(AppEvent::UserMessagesCleared {
                                                channel: channel.clone(),
                                                login: user_login.clone(),
                                            })
                                            .await;
                                    }
                                }
                                EventSubNoticeKind::SuspiciousUserMessage {
                                    user_login,
                                    user_name,
                                    low_trust_status,
                                    ban_evasion_evaluation,
                                    shared_ban_channel_ids,
                                    types,
                                    text,
                                    ..
                                } => {
                                    if low_trust_status.trim().eq_ignore_ascii_case("restricted") {
                                        let mut details = Vec::new();
                                        if types.iter().any(|ty| ty.eq_ignore_ascii_case("ban_evader_detector")) {
                                            let evader = match ban_evasion_evaluation
                                                .as_deref()
                                                .unwrap_or("")
                                                .trim()
                                                .to_ascii_lowercase()
                                                .as_str()
                                            {
                                                "likely" => "likely",
                                                _ => "possible",
                                            };
                                            details.push(format!("Detected as {evader} ban evader"));
                                        }
                                        if !shared_ban_channel_ids.is_empty() {
                                            details.push(format!(
                                                "Banned in {} shared channels",
                                                shared_ban_channel_ids.len()
                                            ));
                                        }

                                        let header_text = if details.is_empty() {
                                            "Suspicious User: Restricted".to_owned()
                                        } else {
                                            format!(
                                                "Suspicious User: Restricted. {}",
                                                details.join(". ")
                                            )
                                        };
                                        let header = make_custom_message(
                                            next_message_id(),
                                            channel.clone(),
                                            header_text,
                                            Utc::now(),
                                            make_sender("", "", ""),
                                            MessageFlags::default(),
                                            MsgKind::SystemInfo,
                                        );
                                        let _ = evt_tx
                                            .send(AppEvent::MessageReceived {
                                                channel: channel.clone(),
                                                message: header,
                                            })
                                            .await;

                                        let body_text = if text.trim().is_empty() {
                                            "[message hidden]".to_owned()
                                        } else {
                                            text.clone()
                                        };
                                        let body = make_custom_message(
                                            next_message_id(),
                                            channel.clone(),
                                            body_text,
                                            Utc::now(),
                                            make_sender("", user_login, user_name),
                                            MessageFlags::default(),
                                            MsgKind::SuspiciousUserMessage,
                                        );
                                        let _ = evt_tx
                                            .send(AppEvent::MessageReceived {
                                                channel: channel.clone(),
                                                message: body,
                                            })
                                            .await;
                                    }
                                }
                                EventSubNoticeKind::SuspiciousUserUpdate {
                                    moderator_user_id,
                                    moderator_login,
                                    moderator_name,
                                    user_name,
                                    low_trust_status,
                                    ..
                                } => {
                                    let action_text = match low_trust_status
                                        .trim()
                                        .to_ascii_lowercase()
                                        .as_str()
                                    {
                                        "restricted" => format!(
                                            "{moderator_name} added {user_name} as a restricted suspicious chatter."
                                        ),
                                        "monitored" => format!(
                                            "{moderator_name} added {user_name} as a monitored suspicious chatter."
                                        ),
                                        "none" => format!(
                                            "{moderator_name} removed {user_name} from the suspicious user list."
                                        ),
                                        other => format!(
                                            "{moderator_name} updated suspicious user status for {user_name} to {other}."
                                        ),
                                    };
                                    let msg = make_custom_message(
                                        next_message_id(),
                                        channel.clone(),
                                        action_text,
                                        Utc::now(),
                                        make_sender(moderator_user_id, moderator_login, moderator_name),
                                        MessageFlags::default(),
                                        MsgKind::SystemInfo,
                                    );
                                    let _ = evt_tx
                                        .send(AppEvent::MessageReceived {
                                            channel: channel.clone(),
                                            message: msg,
                                        })
                                        .await;
                                }
                                EventSubNoticeKind::UserWhisperMessage {
                                    from_user_login,
                                    from_user_name,
                                    to_user_login,
                                    to_user_name: _,
                                    text,
                                    ..
                                } => {
                                    let local_login = auth_username
                                        .as_deref()
                                        .map(str::trim)
                                        .filter(|s| !s.is_empty())
                                        .map(str::to_ascii_lowercase);
                                    let is_self = local_login
                                        .as_deref()
                                        .map(|login| login.eq_ignore_ascii_case(from_user_login))
                                        .unwrap_or(false);
                                    if !is_self && settings.local_log_indexing_enabled {
                                        if let Some(store) = chat_logs.as_ref() {
                                            if let Err(e) = persist_whisper_message(
                                                store,
                                                local_login.as_deref(),
                                                from_user_login,
                                                from_user_name,
                                                to_user_login,
                                                text,
                                                &[],
                                                false,
                                                Utc::now(),
                                            ) {
                                                warn!("whisper-history: failed to persist EventSub whisper: {e}");
                                            }
                                        }
                                    }

                                    let _ = evt_tx
                                        .send(AppEvent::WhisperReceived {
                                            from_login: from_user_login.clone(),
                                            from_display_name: from_user_name.clone(),
                                            target_login: to_user_login.clone(),
                                            text: text.clone(),
                                            twitch_emotes: Vec::new(),
                                            is_self,
                                            timestamp: Utc::now(),
                                            is_history: false,
                                        })
                                        .await;
                                }
                                EventSubNoticeKind::ModerationAction {
                                    action,
                                    target_login,
                                    target_message_id,
                                    ..
                                } => {
                                    if let Some((emote_only, followers_only, slow, subs_only, r9k)) =
                                        room_state_update_from_moderation_action(action)
                                    {
                                        let _ = evt_tx
                                            .send(AppEvent::RoomStateUpdated {
                                                channel: channel.clone(),
                                                emote_only,
                                                followers_only,
                                                slow,
                                                subs_only,
                                                r9k,
                                            })
                                            .await;
                                    }

                                    if let Some(effect) = moderation_action_effect_from_notice(
                                        action,
                                        target_login.as_deref(),
                                        target_message_id.as_deref(),
                                    ) {
                                        match effect {
                                            ModerationActionEffect::ChannelMessagesCleared => {
                                                let _ = evt_tx
                                                    .send(AppEvent::ChannelMessagesCleared {
                                                        channel: channel.clone(),
                                                    })
                                                    .await;
                                            }
                                            ModerationActionEffect::UserMessagesCleared(login) => {
                                                let _ = evt_tx
                                                    .send(AppEvent::UserMessagesCleared {
                                                        channel: channel.clone(),
                                                        login,
                                                    })
                                                    .await;
                                            }
                                            ModerationActionEffect::MessageDeleted(server_id) => {
                                                let _ = evt_tx
                                                    .send(AppEvent::MessageDeleted {
                                                        channel: channel.clone(),
                                                        server_id,
                                                    })
                                                    .await;
                                            }
                                        }
                                    }
                                }
                                EventSubNoticeKind::StreamOnline
                                | EventSubNoticeKind::StreamOffline => {
                                    let is_live =
                                        stream_status_is_live_from_notice(&notice.kind).unwrap_or(false);
                                    let login = notice
                                        .broadcaster_login
                                        .as_deref()
                                        .map(str::trim)
                                        .filter(|s| !s.is_empty())
                                        .map(str::to_ascii_lowercase)
                                        .unwrap_or_else(|| {
                                            channel.display_name().to_ascii_lowercase()
                                        });

                                    if !login.is_empty() {
                                        let _ = evt_tx
                                            .send(AppEvent::StreamStatusUpdated {
                                                login: login.clone(),
                                                is_live,
                                                title: None,
                                                game: None,
                                                viewers: None,
                                            })
                                            .await;

                                        if is_live {
                                            let etx = evt_tx.clone();
                                            let token = settings.oauth_token.clone();
                                            let client_id = helix_client_id.clone();
                                            tokio::spawn(async move {
                                                fetch_twitch_user_profile(
                                                    &login,
                                                    Some(token.as_str()),
                                                    client_id.as_deref(),
                                                    etx,
                                                )
                                                .await;
                                            });
                                        }
                                    }
                                }
                                _ => {}
                            }

                            if should_emit_eventsub_notice_message(&notice.kind) {
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
                        whisper_history_loaded_for = None;
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
                            whisper_history_loaded_for = None;
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
                                    whisper_history_loaded_for = None;
                                    settings.username = String::new();
                                    let _ = eventsub_tx.send(EventSubCommand::ClearAuth).await;
                                    let _ = sess_tx.send(SessionCommand::LogoutAndReconnect).await;
                                    let _ = evt_tx.send(AppEvent::LoggedOut).await;
                                }
                            } else {
                                auth_username = None;
                                auth_user_id = None;
                                whisper_history_loaded_for = None;
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
                        show_timestamp_seconds,
                        use_24h_timestamps,
                        local_log_indexing_enabled,
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
                        settings.show_timestamp_seconds = show_timestamp_seconds;
                        settings.use_24h_timestamps = use_24h_timestamps;
                        settings.local_log_indexing_enabled = local_log_indexing_enabled;
                        settings.auto_join = sanitized_auto_join.clone();
                        settings.highlights = sanitized_highlights.clone();
                        settings.ignores = sanitized_ignores.clone();
                        joined_channels = settings.auto_join.iter().cloned().collect();

                        if let Some(store) = &settings_store {
                            if let Err(e) = store.save(&settings) {
                                warn!("Failed to save general settings: {e}");
                            }
                        }
                        if let Some(host) = plugin_host() {
                            host.set_use_24h_timestamps(settings.use_24h_timestamps);
                        }

                        let _ = evt_tx
                            .send(AppEvent::GeneralSettingsUpdated {
                                show_timestamps: settings.show_timestamps,
                                show_timestamp_seconds: settings.show_timestamp_seconds,
                                use_24h_timestamps: settings.use_24h_timestamps,
                                local_log_indexing_enabled: settings
                                    .local_log_indexing_enabled,
                                auto_join: settings.auto_join.clone(),
                                highlights: settings.highlights.clone(),
                                ignores: settings.ignores.clone(),
                                desktop_notifications_enabled: settings
                                    .desktop_notifications_enabled,
                            })
                            .await;
                    }
                    AppCommand::SetSlashUsageCounts { usage_counts } => {
                        let mut sanitized: BTreeMap<String, u32> = BTreeMap::new();
                        for (raw_name, count) in usage_counts {
                            let key = raw_name
                                .trim()
                                .trim_start_matches('/')
                                .to_ascii_lowercase();
                            if key.is_empty() {
                                continue;
                            }
                            let entry = sanitized.entry(key).or_insert(0);
                            *entry = entry.saturating_add(count);
                        }

                        // Keep only the most-used commands to avoid unbounded growth
                        // from arbitrary slash tokens.
                        const MAX_SLASH_USAGE_ENTRIES: usize = 128;
                        if sanitized.len() > MAX_SLASH_USAGE_ENTRIES {
                            let mut ranked: Vec<(String, u32)> = sanitized.into_iter().collect();
                            ranked.sort_by(|(a_name, a_count), (b_name, b_count)| {
                                b_count.cmp(a_count).then_with(|| a_name.cmp(b_name))
                            });
                            ranked.truncate(MAX_SLASH_USAGE_ENTRIES);
                            ranked.sort_by(|(a_name, _), (b_name, _)| a_name.cmp(b_name));
                            sanitized = ranked.into_iter().collect();
                        }

                        settings.slash_usage_counts = sanitized.clone();
                        if let Some(store) = &settings_store {
                            if let Err(e) = store.save(&settings) {
                                warn!("Failed to save slash usage counts: {e}");
                            }
                        }

                        let _ = evt_tx
                            .send(AppEvent::SlashUsageCountsUpdated {
                                usage_counts: sanitized.into_iter().collect(),
                            })
                            .await;
                    }
                    AppCommand::SetEmotePickerPreferences {
                        favorites,
                        recent,
                        provider_boost,
                    } => {
                        let mut seen_favs: HashSet<String> = HashSet::new();
                        let mut sanitized_favorites: Vec<String> = Vec::new();
                        for raw in favorites {
                            let trimmed = raw.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            let key = trimmed.to_ascii_lowercase();
                            if seen_favs.insert(key) {
                                sanitized_favorites.push(trimmed.to_owned());
                            }
                        }

                        let mut seen_recent: HashSet<String> = HashSet::new();
                        let mut sanitized_recent: Vec<String> = Vec::new();
                        for raw in recent {
                            let trimmed = raw.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            let key = trimmed.to_ascii_lowercase();
                            if seen_recent.insert(key) {
                                sanitized_recent.push(trimmed.to_owned());
                            }
                            if sanitized_recent.len() >= 80 {
                                break;
                            }
                        }

                        let sanitized_boost = provider_boost
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_ascii_lowercase)
                            .filter(|s| {
                                matches!(
                                    s.as_str(),
                                    "twitch" | "7tv" | "bttv" | "ffz" | "emoji"
                                )
                            });

                        settings.emote_picker_favorites = sanitized_favorites.clone();
                        settings.emote_picker_recent = sanitized_recent.clone();
                        settings.emote_picker_provider_boost = sanitized_boost.clone();

                        if let Some(store) = &settings_store {
                            if let Err(e) = store.save(&settings) {
                                warn!("Failed to save emote picker preferences: {e}");
                            }
                        }

                        let _ = evt_tx
                            .send(AppEvent::EmotePickerPreferencesUpdated {
                                favorites: sanitized_favorites,
                                recent: sanitized_recent,
                                provider_boost: sanitized_boost,
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
                    AppCommand::SetHighlightRules { rules } => {
                        settings.highlight_rules = rules.clone();
                        if let Some(store) = &settings_store {
                            if let Err(e) = store.save(&settings) {
                                warn!("Failed to save highlight rules: {e}");
                            }
                        }
                        let _ = evt_tx
                            .send(AppEvent::HighlightRulesUpdated { rules })
                            .await;
                    }
                    AppCommand::SetFilterRecords { records } => {
                        settings.filter_records = records.clone();
                        if let Some(store) = &settings_store {
                            if let Err(e) = store.save(&settings) {
                                warn!("Failed to save filter records: {e}");
                            }
                        }
                        let _ = evt_tx
                            .send(AppEvent::FilterRecordsUpdated { records })
                            .await;
                    }
                    AppCommand::SetModActionPresets { presets } => {
                        settings.mod_action_presets = presets.clone();
                        if let Some(store) = &settings_store {
                            if let Err(e) = store.save(&settings) {
                                warn!("Failed to save mod action presets: {e}");
                            }
                        }
                        let _ = evt_tx
                            .send(AppEvent::ModActionPresetsUpdated { presets })
                            .await;
                    }
                    AppCommand::SetNotificationSettings {
                        desktop_notifications_enabled,
                    } => {
                        settings.desktop_notifications_enabled = desktop_notifications_enabled;
                        if let Some(store) = &settings_store {
                            if let Err(e) = store.save(&settings) {
                                warn!("Failed to save notification settings: {e}");
                            }
                        }
                    }
                    AppCommand::RefreshAuth => {
                        // Re-validate the stored OAuth token without forcing a
                        // full logout/login flow.  If the token is still valid,
                        // this is a no-op.  If invalid, the validate path will
                        // emit AppEvent::AuthExpired so the UI can prompt the user.
                        let active_user = settings.username.trim();
                        let token_opt = if active_user.is_empty() {
                            settings_store.as_ref().and_then(|s| s.load_token())
                        } else {
                            settings_store.as_ref().and_then(|s| {
                                s.load_account_token(active_user).or_else(|| s.load_token())
                            })
                        };

                        if let Some(token) = token_opt {
                            let tx = token_val_tx.clone();
                            tokio::spawn(async move {
                                let result = validate_token(&token).await;
                                let _ = tx
                                    .send(TokenValidationResult::Refresh { token, result })
                                    .await;
                            });
                        } else {
                            let _ = evt_tx
                                .send(AppEvent::AuthExpired)
                                .await;
                        }
                    }
                    AppCommand::SendMessage {
                        channel,
                        text,
                        mut reply_to_msg_id,
                        reply,
                    } => {
                        if channel.is_twitch() {
                            if let Some((target_login, whisper_text)) =
                                parse_outgoing_whisper_command(&text)
                            {
                                let token = settings.oauth_token.clone();
                                let client_id = helix_client_id.clone();
                                let from_user_id = auth_user_id.clone();
                                let from_login = auth_username.clone();
                                let whisper_twitch_emotes = {
                                    let emote_guard = emote_index.read().unwrap();
                                    infer_twitch_emote_positions_from_text(
                                        &emote_guard,
                                        &whisper_text,
                                    )
                                };
                                if settings.local_log_indexing_enabled {
                                    if let Some(store) = chat_logs.as_ref() {
                                        if let Some(sender_login) = from_login
                                            .as_deref()
                                            .and_then(normalize_whisper_login)
                                        {
                                            if let Err(e) = persist_whisper_message(
                                                store,
                                                Some(sender_login.as_str()),
                                                &sender_login,
                                                &sender_login,
                                                &target_login,
                                                &whisper_text,
                                                &whisper_twitch_emotes,
                                                true,
                                                Utc::now(),
                                            ) {
                                                warn!("whisper-history: failed to persist sent whisper: {e}");
                                            }
                                        }
                                    }
                                }
                                let evt_tx2 = evt_tx.clone();
                                tokio::spawn(async move {
                                    helix_send_whisper(
                                        &token,
                                        client_id.as_deref(),
                                        from_user_id.as_deref(),
                                        from_login.as_deref(),
                                        &target_login,
                                        &whisper_text,
                                        whisper_twitch_emotes,
                                        evt_tx2,
                                    )
                                    .await;
                                });
                                continue;
                            }
                        }

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
                    AppCommand::SendWhisper { target_login, text } => {
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        let from_user_id = auth_user_id.clone();
                        let from_login = auth_username.clone();
                        let whisper_text = text.trim().to_owned();
                        let whisper_twitch_emotes = {
                            let emote_guard = emote_index.read().unwrap();
                            infer_twitch_emote_positions_from_text(&emote_guard, &whisper_text)
                        };
                        if settings.local_log_indexing_enabled {
                            if let Some(store) = chat_logs.as_ref() {
                                if !whisper_text.is_empty() {
                                    if let Some(sender_login) = from_login
                                        .as_deref()
                                        .and_then(normalize_whisper_login)
                                    {
                                        if let Err(e) = persist_whisper_message(
                                            store,
                                            Some(sender_login.as_str()),
                                            &sender_login,
                                            &sender_login,
                                            &target_login,
                                            &whisper_text,
                                            &whisper_twitch_emotes,
                                            true,
                                            Utc::now(),
                                        ) {
                                            warn!("whisper-history: failed to persist sent whisper: {e}");
                                        }
                                    }
                                }
                            }
                        }
                        let evt_tx2 = evt_tx.clone();
                        tokio::spawn(async move {
                            helix_send_whisper(
                                &token,
                                client_id.as_deref(),
                                from_user_id.as_deref(),
                                from_login.as_deref(),
                                &target_login,
                                &whisper_text,
                                whisper_twitch_emotes,
                                evt_tx2,
                            )
                            .await;
                        });
                    }
                    AppCommand::FetchUserProfile { login } => {
                        let etx = evt_tx.clone();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        tokio::spawn(async move {
                            fetch_twitch_user_profile(
                                &login,
                                Some(token.as_str()),
                                client_id.as_deref(),
                                etx,
                            )
                            .await;
                        });
                    }
                    AppCommand::FetchStreamStatus { login } => {
                        let etx = evt_tx.clone();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        tokio::spawn(async move {
                            fetch_twitch_stream_status(
                                &login,
                                Some(token.as_str()),
                                client_id.as_deref(),
                                etx,
                            )
                            .await;
                        });
                    }
                    AppCommand::TimeoutUser { channel, login, user_id, seconds, reason } => {
                        if let Some(remaining) = moderation_command_remaining_cooldown(
                            &mut moderation_cmd_cooldowns,
                            &channel,
                            Instant::now(),
                        ) {
                            let _ = evt_tx
                                .send(AppEvent::Error {
                                    context: "Moderation".into(),
                                    message: format!(
                                        "Moderation action is on cooldown. Try again in {}ms.",
                                        remaining.as_millis()
                                    ),
                                })
                                .await;
                            continue;
                        }

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
                    AppCommand::DeleteMessage { channel, message_id } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let moderator_id   = auth_user_id.clone();
                        let token          = settings.oauth_token.clone();
                        let client_id      = helix_client_id.clone();
                        let evt_tx2        = evt_tx.clone();
                        let ch_name        = channel.clone();
                        tokio::spawn(async move {
                            helix_delete_message(
                                &token, client_id.as_deref(),
                                broadcaster_id.as_deref(), moderator_id.as_deref(),
                                &message_id, &ch_name, evt_tx2,
                            ).await;
                        });
                    }
                    AppCommand::ClearUserMessagesLocally { channel, login } => {
                        let _ = evt_tx
                            .send(AppEvent::ClearUserMessagesLocally { channel, login })
                            .await;
                    }
                    AppCommand::BanUser { channel, login, user_id, reason } => {
                        if let Some(remaining) = moderation_command_remaining_cooldown(
                            &mut moderation_cmd_cooldowns,
                            &channel,
                            Instant::now(),
                        ) {
                            let _ = evt_tx
                                .send(AppEvent::Error {
                                    context: "Moderation".into(),
                                    message: format!(
                                        "Moderation action is on cooldown. Try again in {}ms.",
                                        remaining.as_millis()
                                    ),
                                })
                                .await;
                            continue;
                        }

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
                        if let Some(remaining) = moderation_command_remaining_cooldown(
                            &mut moderation_cmd_cooldowns,
                            &channel,
                            Instant::now(),
                        ) {
                            let _ = evt_tx
                                .send(AppEvent::Error {
                                    context: "Moderation".into(),
                                    message: format!(
                                        "Moderation action is on cooldown. Try again in {}ms.",
                                        remaining.as_millis()
                                    ),
                                })
                                .await;
                            continue;
                        }

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
                    AppCommand::WarnUser {
                        channel,
                        login,
                        user_id: _,
                        reason,
                    } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let moderator_id = auth_user_id.clone();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        let evt_tx2 = evt_tx.clone();
                        let ch_name = channel.clone();
                        tokio::spawn(async move {
                            helix_warn_user(
                                &token,
                                client_id.as_deref(),
                                broadcaster_id.as_deref(),
                                moderator_id.as_deref(),
                                &login,
                                &reason,
                                &ch_name,
                                evt_tx2,
                            )
                            .await;
                        });
                    }
                    AppCommand::SetSuspiciousUser {
                        channel,
                        login,
                        user_id: _,
                        restricted,
                    } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let moderator_id = auth_user_id.clone();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        let evt_tx2 = evt_tx.clone();
                        let ch_name = channel.clone();
                        tokio::spawn(async move {
                            helix_set_suspicious_user(
                                &token,
                                client_id.as_deref(),
                                broadcaster_id.as_deref(),
                                moderator_id.as_deref(),
                                &login,
                                restricted,
                                &ch_name,
                                evt_tx2,
                            )
                            .await;
                        });
                    }
                    AppCommand::ClearSuspiciousUser {
                        channel,
                        login,
                        user_id: _,
                    } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let moderator_id = auth_user_id.clone();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        let evt_tx2 = evt_tx.clone();
                        let ch_name = channel.clone();
                        tokio::spawn(async move {
                            helix_clear_suspicious_user(
                                &token,
                                client_id.as_deref(),
                                broadcaster_id.as_deref(),
                                moderator_id.as_deref(),
                                &login,
                                &ch_name,
                                evt_tx2,
                            )
                            .await;
                        });
                    }
                    AppCommand::ResolveAutoModMessage {
                        channel,
                        message_id,
                        sender_user_id,
                        action,
                    } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let moderator_id = auth_user_id.clone();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        let evt_tx2 = evt_tx.clone();
                        let ch_name = channel.clone();
                        tokio::spawn(async move {
                            helix_resolve_automod_message(
                                &token,
                                client_id.as_deref(),
                                broadcaster_id.as_deref(),
                                moderator_id.as_deref(),
                                &message_id,
                                &sender_user_id,
                                &action,
                                &ch_name,
                                evt_tx2,
                            )
                            .await;
                        });
                    }
                    AppCommand::FetchUnbanRequests { channel } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let moderator_id = auth_user_id.clone();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        let evt_tx2 = evt_tx.clone();
                        let ch_name = channel.clone();
                        tokio::spawn(async move {
                            helix_fetch_unban_requests(
                                &token,
                                client_id.as_deref(),
                                broadcaster_id.as_deref(),
                                moderator_id.as_deref(),
                                &ch_name,
                                evt_tx2,
                            )
                            .await;
                        });
                    }
                    AppCommand::ResolveUnbanRequest {
                        channel,
                        request_id,
                        approve,
                        resolution_text,
                    } => {
                        let broadcaster_id = channel_room_ids.get(&channel).cloned();
                        let moderator_id = auth_user_id.clone();
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        let evt_tx2 = evt_tx.clone();
                        let ch_name = channel.clone();
                        tokio::spawn(async move {
                            helix_resolve_unban_request(
                                &token,
                                client_id.as_deref(),
                                broadcaster_id.as_deref(),
                                moderator_id.as_deref(),
                                &request_id,
                                approve,
                                resolution_text.as_deref(),
                                &ch_name,
                                evt_tx2,
                            )
                            .await;
                        });
                    }
                    AppCommand::OpenModerationTools { channel } => {
                        let _ = evt_tx
                            .send(AppEvent::OpenModerationTools { channel })
                            .await;
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
                        channel_points_per_vote,
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
                                channel_points_per_vote,
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
                    AppCommand::ReloadPlugins => {
                        if let Some(host) = plugin_host() {
                            host.reload();
                            local_msg_id += 1;
                            let msg = make_system_message(
                                local_msg_id,
                                ChannelId::new("system"),
                                "Plugins reloaded.".to_owned(),
                                Utc::now(),
                                MsgKind::SystemInfo,
                            );
                            let _ = evt_tx
                                .send(AppEvent::MessageReceived {
                                    channel: ChannelId::new("system"),
                                    message: msg,
                                })
                                .await;
                        } else {
                            let _ = evt_tx
                                .send(AppEvent::Error {
                                    context: "Plugins".into(),
                                    message: "Plugin host is unavailable.".into(),
                                })
                                .await;
                        }
                    }
                    AppCommand::RunPluginCallback {
                        vm_key,
                        callback_ref,
                    } => {
                        if let Some(host) = plugin_host() {
                            host.run_plugin_callback(vm_key, callback_ref);
                        }
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
                        let token = settings.oauth_token.clone();
                        let client_id = helix_client_id.clone();
                        tokio::spawn(async move {
                            fetch_user_profile_for_channel(
                                &login,
                                &channel,
                                Some(token.as_str()),
                                client_id.as_deref(),
                                etx,
                            )
                            .await;
                        });
                    }
                    AppCommand::RunPluginCommand {
                        channel,
                        command,
                        words,
                        reply_to_msg_id,
                        reply,
                        raw_text,
                    } => {
                        if let Some(host) = plugin_host() {
                            host.execute_command(PluginCommandInvocation {
                                command,
                                channel,
                                words,
                                reply_to_msg_id,
                                reply,
                                raw_text,
                            });
                        } else {
                            let _ = evt_tx
                                .send(AppEvent::Error {
                                    context: "Plugins".into(),
                                    message: "Plugin host is unavailable.".into(),
                                })
                                .await;
                        }
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
                        if !settings.local_log_indexing_enabled {
                            let _ = evt_tx
                                .send(AppEvent::Error {
                                    context: "History".into(),
                                    message:
                                        "Local log indexing is disabled in settings.".into(),
                                })
                                .await;
                            continue;
                        }
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
                                warn_missing_whisper_scope(&evt_tx, &info.scopes).await;
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
                                warn_missing_whisper_scope(&evt_tx, &info.scopes).await;
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
                                warn_missing_whisper_scope(&evt_tx, &info.scopes).await;
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
                    TokenValidationResult::Refresh { token, result } => {
                        match result {
                            Ok(info) => {
                                warn_missing_whisper_scope(&evt_tx, &info.scopes).await;
                                let login = info.login;
                                let client_id = info.client_id;

                                if !client_id.is_empty() {
                                    helix_client_id = Some(client_id);
                                }
                                // Keep legacy and active-account token fields in sync.
                                settings.oauth_token = token.clone();
                                if !settings.username.is_empty() {
                                    if let Some(acc) = settings
                                        .accounts
                                        .iter_mut()
                                        .find(|a| a.username == settings.username)
                                    {
                                        acc.oauth_token = token.clone();
                                    }
                                }
                                if let Some(store) = &settings_store {
                                    let _ = store.save(&settings);
                                    if !settings.username.is_empty() {
                                        store.try_save_account_keyring(&settings.username, &token);
                                    }
                                }

                                // If the runtime lost auth state, re-authenticate silently.
                                if auth_username.is_none() {
                                    auth_in_progress = true;
                                    let nick = if login.is_empty() {
                                        settings.username.clone()
                                    } else {
                                        login
                                    };
                                    let _ = sess_tx
                                        .send(SessionCommand::Authenticate { token, nick })
                                        .await;
                                }
                            }
                            Err(ValidateError::Unauthorized) => {
                                let _ = evt_tx.send(AppEvent::AuthExpired).await;
                            }
                            Err(ValidateError::Transient(e)) => {
                                warn!("RefreshAuth transient validation error: {e}");
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

fn moderation_command_remaining_cooldown(
    cooldowns: &mut HashMap<String, Instant>,
    channel: &ChannelId,
    now: Instant,
) -> Option<Duration> {
    let key = channel.as_str().to_ascii_lowercase();
    if let Some(last) = cooldowns.get(&key) {
        let elapsed = now.duration_since(*last);
        if elapsed < MODERATION_CMD_COOLDOWN {
            return Some(MODERATION_CMD_COOLDOWN - elapsed);
        }
    }

    cooldowns.insert(key, now);
    None
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

// Emote and chat history loading helpers live in runtime modules.

// Token validation

/// Call `POST /helix/moderation/bans` to timeout or permanently ban a user.
///
/// `duration_secs` = `None` → permanent ban; `Some(n)` → timeout for `n` seconds.
async fn helix_delete_message(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    moderator_id: Option<&str>,
    message_id: &str,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let (Some(cid), Some(bid), Some(mid)) = (client_id, broadcaster_id, moderator_id) else {
        warn!(
            "helix_delete_message: missing credentials (cid={:?} bid={:?} mid={:?})",
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
        "https://api.twitch.tv/helix/moderation/chat?broadcaster_id={bid}&moderator_id={mid}&message_id={message_id}"
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
            warn!("helix_delete_message: request failed: {e}");
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Moderation".into(),
                    message: format!("Moderation delete request failed: {e}"),
                })
                .await;
            return;
        }
    };

    let status = resp.status();
    if status.as_u16() == 401 {
        let _ = evt_tx.send(AppEvent::AuthExpired).await;
        return;
    }

    if status.is_success() {
        info!("Moderation: deleted message in #{channel}");
        emit_helix_system_info(evt_tx, channel, "Moderation: message deleted.".into()).await;
        return;
    } else {
        let body_text = resp.text().await.unwrap_or_default();
        warn!("helix_delete_message: HTTP {status} - {body_text}");
        let helix_msg = serde_json::from_str::<serde_json::Value>(&body_text)
            .ok()
            .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_owned))
            .unwrap_or_else(|| format!("HTTP {status}"));
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Moderation".into(),
                message: format!("Msg deletion failed: {helix_msg}"),
            })
            .await;
    }
}

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

async fn helix_warn_user(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    moderator_id: Option<&str>,
    target_login: &str,
    reason: &str,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let (Some(cid), Some(bid), Some(mid)) = (client_id, broadcaster_id, moderator_id) else {
        warn!("helix_warn_user: missing credentials");
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Moderation".into(),
                message: "Cannot warn: missing Twitch credentials. Reconnect and try again.".into(),
            })
            .await;
        return;
    };

    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    if bare.trim().is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Moderation".into(),
                message: "You must be logged in to warn a user.".into(),
            })
            .await;
        return;
    }

    let target_login = target_login
        .trim()
        .trim_start_matches('@')
        .to_ascii_lowercase();
    let reason = reason.trim();
    if target_login.is_empty() || reason.is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Moderation".into(),
                message: "Cannot warn: target login and reason are required.".into(),
            })
            .await;
        return;
    }

    let target_user_id = match helix_user_id_by_login(bare, cid, &target_login).await {
        Ok(id) => id,
        Err(msg) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Moderation".into(),
                    message: msg,
                })
                .await;
            return;
        }
    };

    let client = reqwest::Client::new();
    let url = format!(
        "https://api.twitch.tv/helix/moderation/warnings?broadcaster_id={bid}&moderator_id={mid}"
    );
    let resp = match client
        .post(&url)
        .header("Authorization", format!("Bearer {bare}"))
        .header("Client-Id", cid)
        .json(&serde_json::json!({
            "data": {
                "reason": reason,
                "user_id": target_user_id,
            }
        }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("helix_warn_user: request failed: {e}");
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Moderation".into(),
                    message: format!("Warn request failed: {e}"),
                })
                .await;
            return;
        }
    };

    let status = resp.status();
    if status.is_success() {
        info!("Moderation: warned {target_login} in #{channel}");
        return;
    }

    let body_text = resp.text().await.unwrap_or_default();
    warn!("helix_warn_user: HTTP {status} - {body_text}");
    let helix_msg = helix_error_message(status, &body_text);
    let _ = evt_tx
        .send(AppEvent::Error {
            context: "Moderation".into(),
            message: format!("Could not warn {target_login}: {helix_msg}"),
        })
        .await;
}

async fn helix_set_suspicious_user(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    moderator_id: Option<&str>,
    target_login: &str,
    restricted: bool,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let (Some(cid), Some(bid), Some(mid)) = (client_id, broadcaster_id, moderator_id) else {
        warn!("helix_set_suspicious_user: missing credentials");
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Moderation".into(),
                message:
                    "Cannot update low-trust status: missing Twitch credentials. Reconnect and try again."
                        .into(),
            })
            .await;
        return;
    };

    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    if bare.trim().is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Moderation".into(),
                message: "You must be logged in to update low-trust status.".into(),
            })
            .await;
        return;
    }

    let target_login = target_login
        .trim()
        .trim_start_matches('@')
        .to_ascii_lowercase();
    if target_login.is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Moderation".into(),
                message: "Cannot update low-trust status: missing target login.".into(),
            })
            .await;
        return;
    }

    let target_user_id = match helix_user_id_by_login(bare, cid, &target_login).await {
        Ok(id) => id,
        Err(msg) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Moderation".into(),
                    message: msg,
                })
                .await;
            return;
        }
    };

    let client = reqwest::Client::new();
    let url = format!(
        "https://api.twitch.tv/helix/moderation/suspicious_users?broadcaster_id={bid}&moderator_id={mid}"
    );
    let resp = match client
        .post(&url)
        .header("Authorization", format!("Bearer {bare}"))
        .header("Client-Id", cid)
        .json(&serde_json::json!({
            "user_id": target_user_id,
            "status": if restricted { "RESTRICTED" } else { "ACTIVE_MONITORING" },
        }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("helix_set_suspicious_user: request failed: {e}");
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Moderation".into(),
                    message: format!("Low-trust update failed: {e}"),
                })
                .await;
            return;
        }
    };

    let status = resp.status();
    if status.is_success() {
        info!(
            "Moderation: set suspicious user {target_login} ({}) in #{channel}",
            if restricted {
                "restricted"
            } else {
                "monitored"
            }
        );
        return;
    }

    let body_text = resp.text().await.unwrap_or_default();
    warn!("helix_set_suspicious_user: HTTP {status} - {body_text}");
    let helix_msg = helix_error_message(status, &body_text);
    let _ = evt_tx
        .send(AppEvent::Error {
            context: "Moderation".into(),
            message: format!("Could not update low-trust status for {target_login}: {helix_msg}"),
        })
        .await;
}

async fn helix_clear_suspicious_user(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    moderator_id: Option<&str>,
    target_login: &str,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let (Some(cid), Some(bid), Some(mid)) = (client_id, broadcaster_id, moderator_id) else {
        warn!("helix_clear_suspicious_user: missing credentials");
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Moderation".into(),
                message:
                    "Cannot clear low-trust status: missing Twitch credentials. Reconnect and try again."
                        .into(),
            })
            .await;
        return;
    };

    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    if bare.trim().is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Moderation".into(),
                message: "You must be logged in to clear low-trust status.".into(),
            })
            .await;
        return;
    }

    let target_login = target_login
        .trim()
        .trim_start_matches('@')
        .to_ascii_lowercase();
    if target_login.is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Moderation".into(),
                message: "Cannot clear low-trust status: missing target login.".into(),
            })
            .await;
        return;
    }

    let target_user_id = match helix_user_id_by_login(bare, cid, &target_login).await {
        Ok(id) => id,
        Err(msg) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Moderation".into(),
                    message: msg,
                })
                .await;
            return;
        }
    };

    let client = reqwest::Client::new();
    let url = format!(
        "https://api.twitch.tv/helix/moderation/suspicious_users?broadcaster_id={bid}&moderator_id={mid}&user_id={target_user_id}"
    );
    let resp = match client
        .delete(&url)
        .header("Authorization", format!("Bearer {bare}"))
        .header("Client-Id", cid)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("helix_clear_suspicious_user: request failed: {e}");
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Moderation".into(),
                    message: format!("Low-trust removal failed: {e}"),
                })
                .await;
            return;
        }
    };

    let status = resp.status();
    if status.is_success() || status.as_u16() == 204 {
        info!("Moderation: cleared suspicious user {target_login} in #{channel}");
        return;
    }

    let body_text = resp.text().await.unwrap_or_default();
    warn!("helix_clear_suspicious_user: HTTP {status} - {body_text}");
    let helix_msg = helix_error_message(status, &body_text);
    let _ = evt_tx
        .send(AppEvent::Error {
            context: "Moderation".into(),
            message: format!("Could not clear low-trust status for {target_login}: {helix_msg}"),
        })
        .await;
}

/// Resolve a held AutoMod message via
/// `POST /helix/moderation/automod/message`.
async fn helix_resolve_automod_message(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    moderator_id: Option<&str>,
    message_id: &str,
    sender_user_id: &str,
    action: &str,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let (cid, bid, mid) =
        match require_helix_moderation_context(client_id, broadcaster_id, moderator_id) {
            Ok(v) => v,
            Err(msg) => {
                let _ = evt_tx
                    .send(AppEvent::Error {
                        context: "AutoMod".into(),
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
                context: "AutoMod".into(),
                message: "You must be logged in to resolve AutoMod messages.".into(),
            })
            .await;
        return;
    }

    let msg_id = message_id.trim();
    let user_id = sender_user_id.trim();
    if msg_id.is_empty() || user_id.is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "AutoMod".into(),
                message: "Cannot resolve AutoMod item: missing message or user identifier.".into(),
            })
            .await;
        return;
    }

    let action_norm = action.trim().to_ascii_uppercase();
    if action_norm != "ALLOW" && action_norm != "DENY" {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "AutoMod".into(),
                message: format!("Unsupported AutoMod action: {action}"),
            })
            .await;
        return;
    }

    #[derive(serde::Serialize)]
    struct AutoModResolveBody<'a> {
        user_id: &'a str,
        msg_id: &'a str,
        action: &'a str,
    }

    let url = "https://api.twitch.tv/helix/moderation/automod/message";
    let client = reqwest::Client::new();
    let resp = match client
        .post(url)
        .header("Authorization", format!("Bearer {bare}"))
        .header("Client-Id", cid)
        .query(&[("broadcaster_id", bid), ("moderator_id", mid)])
        .json(&AutoModResolveBody {
            user_id,
            msg_id,
            action: &action_norm,
        })
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "AutoMod".into(),
                    message: format!("AutoMod resolve request failed: {e}"),
                })
                .await;
            return;
        }
    };

    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        let _ = evt_tx
            .send(AppEvent::AutoModQueueRemove {
                channel: channel.clone(),
                message_id: msg_id.to_owned(),
                action: Some(action_norm.clone()),
            })
            .await;
        emit_helix_system_info(
            evt_tx,
            channel,
            format!("AutoMod: {action_norm} message {msg_id}."),
        )
        .await;
        return;
    }

    let msg = helix_error_message(status, &body_text);
    let _ = evt_tx
        .send(AppEvent::Error {
            context: "AutoMod".into(),
            message: format!("Could not resolve AutoMod message: {msg}"),
        })
        .await;
}

/// Fetch pending unban requests via
/// `GET /helix/moderation/unban_requests`.
async fn helix_fetch_unban_requests(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    moderator_id: Option<&str>,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let (cid, bid, mid) =
        match require_helix_moderation_context(client_id, broadcaster_id, moderator_id) {
            Ok(v) => v,
            Err(msg) => {
                let _ = evt_tx
                    .send(AppEvent::UnbanRequestsFailed {
                        channel: channel.clone(),
                        error: msg,
                    })
                    .await;
                return;
            }
        };

    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    if bare.trim().is_empty() {
        let msg = "You must be logged in to fetch unban requests.".to_owned();
        let _ = evt_tx
            .send(AppEvent::UnbanRequestsFailed {
                channel: channel.clone(),
                error: msg,
            })
            .await;
        return;
    }

    #[derive(serde::Deserialize)]
    struct HelixUnbanRequest {
        id: String,
        user_id: String,
        user_login: String,
        text: Option<String>,
        created_at: Option<String>,
        status: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct Pagination {
        cursor: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct HelixUnbanRequestsResponse {
        data: Vec<HelixUnbanRequest>,
        #[serde(default)]
        pagination: Option<Pagination>,
    }

    let client = reqwest::Client::new();
    let mut cursor: Option<String> = None;
    let mut requests: Vec<UnbanRequestItem> = Vec::new();

    loop {
        let mut query: Vec<(&str, String)> = vec![
            ("broadcaster_id", bid.to_owned()),
            ("moderator_id", mid.to_owned()),
            ("status", "pending".to_owned()),
            ("first", "100".to_owned()),
        ];
        if let Some(cur) = cursor.as_deref().filter(|s| !s.is_empty()) {
            query.push(("after", cur.to_owned()));
        }

        let resp = match client
            .get("https://api.twitch.tv/helix/moderation/unban_requests")
            .header("Authorization", format!("Bearer {bare}"))
            .header("Client-Id", cid)
            .query(&query)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = evt_tx
                    .send(AppEvent::UnbanRequestsFailed {
                        channel: channel.clone(),
                        error: format!("Unban request fetch failed: {e}"),
                    })
                    .await;
                return;
            }
        };

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let _ = evt_tx
                .send(AppEvent::UnbanRequestsFailed {
                    channel: channel.clone(),
                    error: helix_error_message(status, &body),
                })
                .await;
            return;
        }

        let parsed = match serde_json::from_str::<HelixUnbanRequestsResponse>(&body) {
            Ok(v) => v,
            Err(e) => {
                let _ = evt_tx
                    .send(AppEvent::UnbanRequestsFailed {
                        channel: channel.clone(),
                        error: format!("Failed to parse unban request response: {e}"),
                    })
                    .await;
                return;
            }
        };

        requests.extend(parsed.data.into_iter().map(|item| UnbanRequestItem {
            request_id: item.id,
            user_id: item.user_id,
            user_login: item.user_login,
            text: item.text,
            created_at: item.created_at,
            status: item.status,
        }));

        cursor = parsed.pagination.and_then(|p| p.cursor);
        if cursor.as_deref().map_or(true, |s| s.is_empty()) {
            break;
        }
    }

    let _ = evt_tx
        .send(AppEvent::UnbanRequestsLoaded {
            channel: channel.clone(),
            requests,
        })
        .await;
}

/// Resolve an unban request via
/// `PATCH /helix/moderation/unban_requests`.
async fn helix_resolve_unban_request(
    token: &str,
    client_id: Option<&str>,
    broadcaster_id: Option<&str>,
    moderator_id: Option<&str>,
    request_id: &str,
    approve: bool,
    resolution_text: Option<&str>,
    channel: &ChannelId,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    let (cid, bid, mid) =
        match require_helix_moderation_context(client_id, broadcaster_id, moderator_id) {
            Ok(v) => v,
            Err(msg) => {
                let _ = evt_tx
                    .send(AppEvent::Error {
                        context: "Unban Requests".into(),
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
                context: "Unban Requests".into(),
                message: "You must be logged in to resolve unban requests.".into(),
            })
            .await;
        return;
    }

    let request_id = request_id.trim();
    if request_id.is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Unban Requests".into(),
                message: "Missing unban request id.".into(),
            })
            .await;
        return;
    }

    let status_raw = if approve { "approved" } else { "denied" };
    let status_emit = status_raw.to_ascii_uppercase();

    #[derive(serde::Serialize)]
    struct ResolveUnbanBody<'a> {
        unban_request_id: &'a str,
        status: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        resolution_text: Option<&'a str>,
    }

    let trimmed_resolution = resolution_text.map(str::trim).filter(|s| !s.is_empty());

    let req = ResolveUnbanBody {
        unban_request_id: request_id,
        status: status_raw,
        resolution_text: trimmed_resolution,
    };

    let client = reqwest::Client::new();
    let resp = match client
        .patch("https://api.twitch.tv/helix/moderation/unban_requests")
        .header("Authorization", format!("Bearer {bare}"))
        .header("Client-Id", cid)
        .query(&[("broadcaster_id", bid), ("moderator_id", mid)])
        .json(&req)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Unban Requests".into(),
                    message: format!("Resolve unban request failed: {e}"),
                })
                .await;
            return;
        }
    };

    let http_status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    if http_status.is_success() {
        let _ = evt_tx
            .send(AppEvent::UnbanRequestResolved {
                channel: channel.clone(),
                request_id: request_id.to_owned(),
                status: status_emit.clone(),
            })
            .await;
        return;
    }

    let msg = helix_error_message(http_status, &body_text);
    let _ = evt_tx
        .send(AppEvent::Error {
            context: "Unban Requests".into(),
            message: format!("Could not resolve unban request: {msg}"),
        })
        .await;
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
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_owned))
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

fn parse_outgoing_whisper_command(text: &str) -> Option<(String, String)> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let without_slash = trimmed.strip_prefix('/')?;
    let (cmd, rest) = without_slash
        .split_once(char::is_whitespace)
        .map(|(c, r)| (c.trim().to_ascii_lowercase(), r.trim()))
        .unwrap_or_else(|| (without_slash.trim().to_ascii_lowercase(), ""));
    if !matches!(cmd.as_str(), "w" | "whisper") {
        return None;
    }

    let (raw_target, raw_message) = rest
        .split_once(char::is_whitespace)
        .map(|(target, message)| (target.trim(), message.trim()))
        .unwrap_or((rest.trim(), ""));
    if raw_target.is_empty() || raw_message.is_empty() {
        return None;
    }

    let target_login = raw_target
        .trim_start_matches('@')
        .trim_start_matches('#')
        .to_ascii_lowercase();
    let valid_target = {
        let len = target_login.len();
        (3..=25).contains(&len)
            && target_login
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    };
    if !valid_target {
        return None;
    }

    Some((target_login, raw_message.to_owned()))
}

fn normalize_whisper_login(login: &str) -> Option<String> {
    let login = login
        .trim()
        .trim_start_matches('@')
        .trim_start_matches('#')
        .to_ascii_lowercase();
    let len = login.len();
    if !(3..=25).contains(&len) {
        return None;
    }
    if !login
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return None;
    }
    Some(login)
}

fn whisper_thread_channel_id(partner_login: &str) -> ChannelId {
    ChannelId(format!("{WHISPER_HISTORY_CHANNEL_PREFIX}{partner_login}"))
}

fn infer_twitch_emote_positions_from_text(
    idx: &std::collections::HashMap<String, EmoteInfo>,
    text: &str,
) -> Vec<TwitchEmotePos> {
    let mut out = Vec::new();
    let mut cursor_chars = 0usize;

    for word in text.split_inclusive(' ') {
        let word_char_len = word.chars().count();
        let trimmed = word.trim();
        if !trimmed.is_empty() {
            let leading_ws = word.chars().take_while(|c| c.is_whitespace()).count();
            let trimmed_len = trimmed.chars().count();
            if trimmed_len > 0 {
                if let Some(info) = idx.get(&emote_key("twitch", trimmed)) {
                    let start = cursor_chars + leading_ws;
                    let end = start + trimmed_len - 1;
                    out.push(TwitchEmotePos {
                        id: info.id.clone(),
                        start,
                        end,
                    });
                }
            }
        }
        cursor_chars += word_char_len;
    }

    out
}

fn persist_whisper_message(
    log_store: &LogStore,
    local_login: Option<&str>,
    from_login: &str,
    from_display_name: &str,
    target_login: &str,
    text: &str,
    twitch_emotes: &[TwitchEmotePos],
    is_self: bool,
    timestamp: chrono::DateTime<Utc>,
) -> Result<(), crust_storage::StorageError> {
    let from_login = normalize_whisper_login(from_login);
    let target_login = normalize_whisper_login(target_login);
    let local_login = local_login.and_then(normalize_whisper_login);

    let partner_login = if is_self {
        target_login
            .clone()
            .or_else(|| from_login.clone())
            .or(local_login)
    } else {
        from_login.clone().or(target_login.clone()).or(local_login)
    };
    let Some(partner_login) = partner_login else {
        return Ok(());
    };

    let sender_login = from_login.unwrap_or_else(|| partner_login.clone());
    let sender_display_name = if from_display_name.trim().is_empty() {
        sender_login.clone()
    } else {
        from_display_name.trim().to_owned()
    };

    let msg = ChatMessage {
        id: MessageId(0),
        server_id: None,
        timestamp,
        channel: whisper_thread_channel_id(&partner_login),
        sender: Sender {
            user_id: UserId(sender_login.clone()),
            login: sender_login,
            display_name: sender_display_name,
            color: None,
            name_paint: None,
            badges: Vec::new(),
        },
        raw_text: text.trim().to_owned(),
        spans: Default::default(),
        twitch_emotes: twitch_emotes.to_vec(),
        flags: MessageFlags {
            is_action: false,
            is_highlighted: false,
            is_deleted: false,
            is_first_msg: false,
            is_pinned: false,
            is_self,
            is_mention: false,
            custom_reward_id: None,
            is_history: false,
        },
        reply: None,
        msg_kind: MsgKind::Chat,
    };

    log_store.append_message(&msg)
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

async fn helix_send_whisper(
    token: &str,
    client_id: Option<&str>,
    from_user_id: Option<&str>,
    from_login: Option<&str>,
    target_login: &str,
    text: &str,
    twitch_emotes: Vec<TwitchEmotePos>,
    evt_tx: mpsc::Sender<AppEvent>,
) {
    info!("Whisper send requested to {}", target_login.trim());
    let cid = match client_id {
        Some(cid) => cid,
        None => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Whisper".into(),
                    message: "Missing Twitch Client-ID.".into(),
                })
                .await;
            return;
        }
    };

    let sender_id = match from_user_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => id,
        None => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Whisper".into(),
                    message: "You must be logged in to send whispers.".into(),
                })
                .await;
            return;
        }
    };

    let bare = token.strip_prefix("oauth:").unwrap_or(token);
    if bare.trim().is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Whisper".into(),
                message: "You must be logged in to send whispers.".into(),
            })
            .await;
        return;
    }

    let login = target_login
        .trim()
        .trim_start_matches('@')
        .to_ascii_lowercase();
    if login.is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Whisper".into(),
                message: "Usage: /w <user> <message>".into(),
            })
            .await;
        return;
    }

    if from_login
        .map(|current| current.eq_ignore_ascii_case(&login))
        .unwrap_or(false)
    {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Whisper".into(),
                message: "You cannot whisper yourself.".into(),
            })
            .await;
        return;
    }

    let trimmed_text = text.trim();
    if trimmed_text.is_empty() {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Whisper".into(),
                message: "Whisper message cannot be empty.".into(),
            })
            .await;
        return;
    }
    if trimmed_text.chars().count() > TWITCH_MAX_MESSAGE_CHARS {
        let _ = evt_tx
            .send(AppEvent::Error {
                context: "Whisper".into(),
                message: format!("Whisper too long (>{TWITCH_MAX_MESSAGE_CHARS} characters)."),
            })
            .await;
        return;
    }

    let to_user_id = match helix_user_id_by_login(bare, cid, &login).await {
        Ok(id) => id,
        Err(msg) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Whisper".into(),
                    message: msg,
                })
                .await;
            return;
        }
    };

    #[derive(serde::Serialize)]
    struct WhisperBody<'a> {
        message: &'a str,
    }

    let client = reqwest::Client::new();
    let resp = match client
        .post("https://api.twitch.tv/helix/whispers")
        .header("Authorization", format!("Bearer {bare}"))
        .header("Client-Id", cid)
        .query(&[
            ("from_user_id", sender_id),
            ("to_user_id", to_user_id.as_str()),
        ])
        .json(&WhisperBody {
            message: trimmed_text,
        })
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = evt_tx
                .send(AppEvent::Error {
                    context: "Whisper".into(),
                    message: format!("Send whisper request failed: {e}"),
                })
                .await;
            return;
        }
    };

    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        let sender_login = from_login
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| sender_id.to_owned());
        let now = Utc::now();
        info!("Whisper send succeeded to {login}");
        let _ = evt_tx
            .send(AppEvent::WhisperReceived {
                from_login: sender_login.clone(),
                from_display_name: sender_login,
                target_login: login,
                text: trimmed_text.to_owned(),
                twitch_emotes,
                is_self: true,
                timestamp: now,
                is_history: false,
            })
            .await;
        return;
    }

    let mut msg = helix_error_message(status, &body_text);
    if body_text.contains("MISSING_REQUIRED_SCOPE")
        || msg.to_ascii_lowercase().contains("missing required scope")
    {
        msg.push_str(" Re-login with a token that includes user:manage:whispers.");
    }
    warn!("Whisper send failed to {login}: {msg}");
    let _ = evt_tx
        .send(AppEvent::Error {
            context: "Whisper".into(),
            message: format!("Could not send whisper to {login}: {msg}"),
        })
        .await;
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
        .filter(|c| {
            matches!(
                c.as_str(),
                "primary" | "blue" | "green" | "orange" | "purple"
            )
        });

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

    let login = target_login
        .trim()
        .trim_start_matches('@')
        .to_ascii_lowercase();
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
        emit_helix_system_info(evt_tx, channel, format!("Sent shoutout to {login}.")).await;
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
        let confirmed_len = info.as_ref().and_then(|d| d.length).unwrap_or(length_secs);
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
    channel_points_per_vote: Option<u32>,
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
        channel_points_voting_enabled: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        channel_points_per_vote: Option<u32>,
    }

    let points_per_vote = channel_points_per_vote
        .map(|v| v.clamp(1, 1_000_000))
        .filter(|v| *v > 0);

    let req = PollBody {
        broadcaster_id: bid,
        title,
        choices: choices.iter().map(|c| PollChoice { title: c }).collect(),
        duration: duration_secs,
        channel_points_voting_enabled: points_per_vote.is_some(),
        channel_points_per_vote: points_per_vote,
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
        let _ = points_per_vote;
        let _ = choices;
        let _ = duration_secs;
        emit_helix_system_info(evt_tx, channel, format!("Created poll: '{title}'")).await;
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

#[derive(Debug, Clone, serde::Deserialize)]
struct HelixPollChoiceSummary {
    title: String,
    #[serde(default)]
    votes: u64,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct HelixPollSummary {
    title: String,
    #[serde(default)]
    choices: Vec<HelixPollChoiceSummary>,
}

fn format_completed_poll_message(poll: &HelixPollSummary) -> String {
    let total_votes: u64 = poll.choices.iter().map(|choice| choice.votes).sum();
    if total_votes == 0 {
        return format!("Poll ended with zero votes: '{}'", poll.title);
    }

    let Some(max_votes) = poll.choices.iter().map(|choice| choice.votes).max() else {
        return format!("Poll ended with zero votes: '{}'", poll.title);
    };

    let winners: Vec<&HelixPollChoiceSummary> = poll
        .choices
        .iter()
        .filter(|choice| choice.votes == max_votes)
        .collect();
    if winners.len() != 1 {
        return format!("Poll ended in a draw: '{}'", poll.title);
    }

    let winner = winners[0];
    let percent = 100.0 * winner.votes as f64 / total_votes as f64;
    format!(
        "Ended poll: '{}' - '{}' won with {} votes ({percent:.1}%)",
        poll.title, winner.title, winner.votes
    )
}

fn parse_ended_poll_summary(body: &str) -> Option<HelixPollSummary> {
    #[derive(serde::Deserialize)]
    struct PollSummaryResponse {
        data: Vec<HelixPollSummary>,
    }

    serde_json::from_str::<PollSummaryResponse>(body)
        .ok()?
        .data
        .into_iter()
        .next()
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

    let url = format!("https://api.twitch.tv/helix/polls?broadcaster_id={broadcaster_id}&first=20");
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
    if parsed.data.is_empty() {
        return Err("Failed to find any polls".to_owned());
    }

    let active = parsed
        .data
        .into_iter()
        .find(|p| p.status.eq_ignore_ascii_case("ACTIVE"))
        .ok_or_else(|| "Could not find an active poll".to_owned())?;
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
        let body_text = resp.text().await.unwrap_or_default();
        let message = if status.eq_ignore_ascii_case("TERMINATED") {
            let title = parse_ended_poll_summary(&body_text)
                .map(|poll| poll.title)
                .unwrap_or_else(|| poll_title.clone());
            format!("Canceled poll: '{title}'")
        } else if let Some(poll) = parse_ended_poll_summary(&body_text) {
            format_completed_poll_message(&poll)
        } else {
            format!("Ended poll: '{poll_title}'")
        };
        emit_helix_system_info(evt_tx, channel, message).await;
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

    let url =
        format!("https://api.twitch.tv/helix/predictions?broadcaster_id={broadcaster_id}&first=20");
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
                message: format!("Prediction is {} and cannot be locked.", prediction.status),
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
            format!(
                "Resolved prediction: {} (winner: {})",
                prediction.title, winner
            )
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
    /// OAuth scopes currently granted by this token.
    scopes: Vec<String>,
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
        #[serde(default)]
        scopes: Vec<String>,
    }

    let body = resp.json::<ValidateResponse>().await.map_err(|e| {
        ValidateError::Transient(format!("Failed to parse validation response: {e}"))
    })?;

    Ok(ValidateInfo {
        login: body.login,
        user_id: body.user_id,
        client_id: body.client_id,
        scopes: body.scopes,
    })
}

fn token_has_scope(scopes: &[String], required: &str) -> bool {
    scopes
        .iter()
        .any(|scope| scope.eq_ignore_ascii_case(required))
}

async fn warn_missing_whisper_scope(evt_tx: &mpsc::Sender<AppEvent>, scopes: &[String]) {
    if token_has_scope(scopes, "user:manage:whispers") {
        return;
    }

    let _ = evt_tx
        .send(AppEvent::Error {
            context: "Whisper".into(),
            message: "Your token is missing scope user:manage:whispers, so /w and /whisper will fail. Re-login with that scope enabled.".into(),
        })
        .await;
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
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use super::{
        format_completed_poll_message, format_eventsub_notice_text,
        moderation_action_effect_from_notice, moderation_command_remaining_cooldown,
        parse_twitch_pinned_snapshot_json, should_drop_duplicate_eventsub_notice,
        should_emit_eventsub_notice_message, stream_status_is_live_from_notice,
        HelixPollChoiceSummary, HelixPollSummary, ModerationActionEffect,
        APP_INITIAL_INNER_SIZE, APP_MIN_INNER_SIZE, MODERATION_CMD_COOLDOWN,
    };
    use crate::runtime::system_messages::is_twitch_pinned_notice;
    use crust_core::model::ChannelId;
    use crust_twitch::eventsub::EventSubNoticeKind;

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

    #[test]
    fn moderation_action_effect_maps_clear_to_channel_clear() {
        let effect = moderation_action_effect_from_notice("clear", None, None);
        assert_eq!(effect, Some(ModerationActionEffect::ChannelMessagesCleared));
    }

    #[test]
    fn moderation_action_effect_maps_delete_to_message_deleted() {
        let effect = moderation_action_effect_from_notice("delete", None, Some("msg-1"));
        assert_eq!(
            effect,
            Some(ModerationActionEffect::MessageDeleted("msg-1".to_owned()))
        );
    }

    #[test]
    fn moderation_action_effect_maps_timeout_to_user_clear() {
        let effect = moderation_action_effect_from_notice("timeout_600s", Some("viewer"), None);
        assert_eq!(
            effect,
            Some(ModerationActionEffect::UserMessagesCleared(
                "viewer".to_owned()
            ))
        );
    }

    #[test]
    fn eventsub_notice_dedup_drops_repeated_notice_ids_within_window() {
        let mut seen: HashMap<String, Instant> = HashMap::new();
        let mut gc_at = Instant::now();
        let now = Instant::now();

        assert!(!should_drop_duplicate_eventsub_notice(
            &mut seen, "evt-123", now, &mut gc_at,
        ));
        assert!(should_drop_duplicate_eventsub_notice(
            &mut seen,
            "evt-123",
            now + Duration::from_secs(1),
            &mut gc_at,
        ));
    }

    #[test]
    fn eventsub_notice_dedup_allows_same_id_after_window_expires() {
        let mut seen: HashMap<String, Instant> = HashMap::new();
        let mut gc_at = Instant::now() - Duration::from_secs(20);
        let now = Instant::now();

        assert!(!should_drop_duplicate_eventsub_notice(
            &mut seen, "evt-123", now, &mut gc_at,
        ));
        assert!(!should_drop_duplicate_eventsub_notice(
            &mut seen,
            "evt-123",
            now + Duration::from_secs(45) + Duration::from_secs(1),
            &mut gc_at,
        ));
    }

    #[test]
    fn moderation_notice_text_includes_shared_chat_source_channel() {
        let text = format_eventsub_notice_text(&EventSubNoticeKind::ModerationAction {
            moderator_login: "mod_jane".to_owned(),
            action: "ban".to_owned(),
            target_login: Some("viewer123".to_owned()),
            target_message_id: None,
            source_channel_login: Some("partner_stream".to_owned()),
        });

        assert!(text.contains("mod_jane performed ban on viewer123 in #partner_stream."));
    }

    #[test]
    fn eventsub_notice_emission_suppresses_overlap_with_irc_moderation() {
        assert!(!should_emit_eventsub_notice_message(
            &EventSubNoticeKind::ChannelBan {
                user_login: "viewer".to_owned(),
                reason: None,
                ends_at: None,
            }
        ));
        assert!(!should_emit_eventsub_notice_message(
            &EventSubNoticeKind::ChannelUnban {
                user_login: "viewer".to_owned(),
            }
        ));
        assert!(should_emit_eventsub_notice_message(
            &EventSubNoticeKind::UnbanRequestResolve {
                request_id: "req-1".to_owned(),
                status: "APPROVED".to_owned(),
            }
        ));
        assert!(should_emit_eventsub_notice_message(
            &EventSubNoticeKind::UnbanRequestCreate {
                request_id: "req-1".to_owned(),
                user_id: "u1".to_owned(),
                user_login: "viewer".to_owned(),
                text: Some("please unban".to_owned()),
                created_at: Some("2026-03-31T19:35:00Z".to_owned()),
            }
        ));
    }

    #[test]
    fn stream_status_helper_maps_online_and_offline_notices() {
        assert_eq!(
            stream_status_is_live_from_notice(&EventSubNoticeKind::StreamOnline),
            Some(true)
        );
        assert_eq!(
            stream_status_is_live_from_notice(&EventSubNoticeKind::StreamOffline),
            Some(false)
        );
        assert_eq!(
            stream_status_is_live_from_notice(&EventSubNoticeKind::ChannelUnban {
                user_login: "viewer".to_owned()
            }),
            None
        );
    }

    #[test]
    fn moderation_cooldown_blocks_rapid_repeated_actions_in_same_channel() {
        let mut cooldowns = HashMap::new();
        let channel = ChannelId::new("somechannel");
        let now = Instant::now();

        assert_eq!(
            moderation_command_remaining_cooldown(&mut cooldowns, &channel, now),
            None
        );
        let remaining = moderation_command_remaining_cooldown(
            &mut cooldowns,
            &channel,
            now + Duration::from_millis(100),
        );
        assert!(remaining.is_some());
        assert!(remaining.unwrap() <= MODERATION_CMD_COOLDOWN);
    }

    #[test]
    fn moderation_cooldown_is_per_channel() {
        let mut cooldowns = HashMap::new();
        let chan_a = ChannelId::new("one");
        let chan_b = ChannelId::new("two");
        let now = Instant::now();

        assert_eq!(
            moderation_command_remaining_cooldown(&mut cooldowns, &chan_a, now),
            None
        );
        assert_eq!(
            moderation_command_remaining_cooldown(
                &mut cooldowns,
                &chan_b,
                now + Duration::from_millis(100),
            ),
            None
        );
    }

    #[test]
    fn completed_poll_message_reports_zero_votes() {
        let poll = HelixPollSummary {
            title: "Best snack".to_owned(),
            choices: vec![
                HelixPollChoiceSummary {
                    title: "Chips".to_owned(),
                    votes: 0,
                },
                HelixPollChoiceSummary {
                    title: "Popcorn".to_owned(),
                    votes: 0,
                },
            ],
        };

        assert_eq!(
            format_completed_poll_message(&poll),
            "Poll ended with zero votes: 'Best snack'"
        );
    }

    #[test]
    fn completed_poll_message_reports_draws() {
        let poll = HelixPollSummary {
            title: "Best snack".to_owned(),
            choices: vec![
                HelixPollChoiceSummary {
                    title: "Chips".to_owned(),
                    votes: 5,
                },
                HelixPollChoiceSummary {
                    title: "Popcorn".to_owned(),
                    votes: 5,
                },
            ],
        };

        assert_eq!(
            format_completed_poll_message(&poll),
            "Poll ended in a draw: 'Best snack'"
        );
    }

    #[test]
    fn completed_poll_message_reports_winner_like_chatterino() {
        let poll = HelixPollSummary {
            title: "Best snack".to_owned(),
            choices: vec![
                HelixPollChoiceSummary {
                    title: "Chips".to_owned(),
                    votes: 7,
                },
                HelixPollChoiceSummary {
                    title: "Popcorn".to_owned(),
                    votes: 3,
                },
            ],
        };

        assert_eq!(
            format_completed_poll_message(&poll),
            "Ended poll: 'Best snack' - 'Chips' won with 7 votes (70.0%)"
        );
    }
}

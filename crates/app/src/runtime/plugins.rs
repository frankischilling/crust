#![allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    unused_imports,
    unused_unsafe
)]

use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_double, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use chrono::Utc;
use directories::ProjectDirs;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crust_core::events::{
    AppCommand, AppEvent, AutoModQueueItem, ConnectionState, IvrLogEntry, LinkPreview,
    UnbanRequestItem,
};
use crust_core::highlight::HighlightRule;
use crust_core::model::{
    Badge, ChannelId, ChatMessage, EmoteCatalogEntry, FilterRecord, MessageFlags, ModActionPreset,
    MsgKind, Platform, ReplyInfo, Sender, SenderNamePaint, SenderNamePaintShadow,
    SenderNamePaintStop, Span, SystemNotice, TwitchEmotePos, UserProfile,
};
use crust_core::plugins::{
    PluginAuthSnapshot, PluginChannelSnapshot, PluginCommandInfo, PluginCommandInvocation,
    PluginCompletionList, PluginCompletionRequest, PluginHost, PluginManifestInfo, PluginStatus,
    PluginUiChoice, PluginUiHostPanelRegistration, PluginUiHostPanelSpec, PluginUiHostSlot,
    PluginUiSettingsPageRegistration, PluginUiSettingsPageSpec, PluginUiSnapshot, PluginUiStyle,
    PluginUiSurfaceKind, PluginUiTableColumn, PluginUiValue, PluginUiWidget,
    PluginUiWindowRegistration, PluginUiWindowSpec,
};

use super::system_messages::make_system_message;

static PLUGIN_HOST: OnceLock<RwLock<Option<Arc<LuaPluginHost>>>> = OnceLock::new();
static PLUGIN_STATE_INDEX: OnceLock<RwLock<HashMap<usize, usize>>> = OnceLock::new();

fn plugin_host_cell() -> &'static RwLock<Option<Arc<LuaPluginHost>>> {
    PLUGIN_HOST.get_or_init(|| RwLock::new(None))
}

fn set_global_host(host: Arc<LuaPluginHost>) {
    *plugin_host_cell()
        .write()
        .unwrap_or_else(|p| p.into_inner()) = Some(host);
}

fn global_host() -> Option<Arc<LuaPluginHost>> {
    plugin_host_cell()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

fn plugin_state_index() -> &'static RwLock<HashMap<usize, usize>> {
    PLUGIN_STATE_INDEX.get_or_init(|| RwLock::new(HashMap::new()))
}

#[allow(non_camel_case_types)]
pub enum lua_State {}

type lua_Integer = i64;
type lua_Number = c_double;
type lua_CFunction = Option<unsafe extern "C" fn(*mut lua_State) -> c_int>;

const LUA_OK: c_int = 0;
const LUA_MULTRET: c_int = -1;
const LUA_REGISTRYINDEX: c_int = -((i32::MAX / 2) + 1000);
const LUA_NOREF: c_int = -2;
const LUA_REFNIL: c_int = -1;

const LUA_TNIL: c_int = 0;
const LUA_TBOOLEAN: c_int = 1;
const LUA_TNUMBER: c_int = 3;
const LUA_TSTRING: c_int = 4;
const LUA_TTABLE: c_int = 5;
const LUA_TFUNCTION: c_int = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PluginEventKind {
    CompletionRequested,
    EmoteImageReady,
    Authenticated,
    EmoteCatalogUpdated,
    LoggedOut,
    AccountListUpdated,
    ChannelJoined,
    ChannelParted,
    ChannelRedirected,
    ConnectionStateChanged,
    MessageReceived,
    WhisperReceived,
    MessageDeleted,
    SystemNotice,
    Error,
    HistoryLoaded,
    UserProfileLoaded,
    UserProfileUnavailable,
    StreamStatusUpdated,
    IvrLogsLoaded,
    IvrLogsFailed,
    ChannelEmotesLoaded,
    BetaFeaturesUpdated,
    ChatUiBehaviorUpdated,
    GeneralSettingsUpdated,
    SlashUsageCountsUpdated,
    EmotePickerPreferencesUpdated,
    AppearanceSettingsUpdated,
    FontSettingsUpdated,
    RestoreLastActiveChannel,
    RoomStateUpdated,
    AutoModQueueAppend,
    AutoModQueueRemove,
    UnbanRequestsLoaded,
    UnbanRequestsFailed,
    UnbanRequestUpsert,
    UnbanRequestResolved,
    OpenModerationTools,
    HighlightRulesUpdated,
    FilterRecordsUpdated,
    ModActionPresetsUpdated,
    NicknamesUpdated,
    IgnoredUsersUpdated,
    IgnoredPhrasesUpdated,
    UserPronounsLoaded,
    UsercardSettingsUpdated,
    SelfAvatarLoaded,
    LinkPreviewReady,
    SenderCosmeticsUpdated,
    IrcTopicChanged,
    AuthExpired,
    UserMessagesCleared,
    LowTrustStatusUpdated,
    UserStateUpdated,
    ChannelMessagesCleared,
    ClearUserMessagesLocally,
    ImagePrefetchQueued,
    PluginUiAction,
    PluginUiChange,
    PluginUiSubmit,
    PluginUiWindowClosed,
}

#[link(name = "lua", kind = "static")]
extern "C" {
    fn luaL_newstate() -> *mut lua_State;
    fn lua_close(L: *mut lua_State);
    fn luaL_openselectedlibs(L: *mut lua_State, load: c_int, preload: c_int);
    fn luaL_loadfilex(L: *mut lua_State, filename: *const c_char, mode: *const c_char) -> c_int;
    fn lua_pcallk(
        L: *mut lua_State,
        nargs: c_int,
        nresults: c_int,
        errfunc: c_int,
        ctx: lua_Integer,
        k: lua_CFunction,
    ) -> c_int;
    fn lua_gettop(L: *mut lua_State) -> c_int;
    fn lua_settop(L: *mut lua_State, idx: c_int);
    fn lua_absindex(L: *mut lua_State, idx: c_int) -> c_int;
    fn lua_type(L: *mut lua_State, idx: c_int) -> c_int;
    fn lua_typename(L: *mut lua_State, tp: c_int) -> *const c_char;
    fn lua_toboolean(L: *mut lua_State, idx: c_int) -> c_int;
    fn lua_tointegerx(L: *mut lua_State, idx: c_int, isnum: *mut c_int) -> lua_Integer;
    fn lua_tonumberx(L: *mut lua_State, idx: c_int, isnum: *mut c_int) -> lua_Number;
    fn lua_tolstring(L: *mut lua_State, idx: c_int, len: *mut usize) -> *const c_char;
    fn lua_pushnil(L: *mut lua_State);
    fn lua_pushboolean(L: *mut lua_State, b: c_int);
    fn lua_pushinteger(L: *mut lua_State, n: lua_Integer);
    fn lua_pushnumber(L: *mut lua_State, n: lua_Number);
    fn lua_pushstring(L: *mut lua_State, s: *const c_char) -> *const c_char;
    fn lua_pushlstring(L: *mut lua_State, s: *const c_char, len: usize) -> *const c_char;
    fn lua_pushvalue(L: *mut lua_State, idx: c_int);
    fn lua_pushcclosure(L: *mut lua_State, f: lua_CFunction, n: c_int);
    fn lua_rotate(L: *mut lua_State, idx: c_int, n: c_int);
    fn lua_createtable(L: *mut lua_State, narr: c_int, nrec: c_int);
    fn lua_getglobal(L: *mut lua_State, name: *const c_char) -> c_int;
    fn lua_setglobal(L: *mut lua_State, name: *const c_char);
    fn lua_getfield(L: *mut lua_State, idx: c_int, k: *const c_char) -> c_int;
    fn lua_setfield(L: *mut lua_State, idx: c_int, k: *const c_char);
    fn lua_rawgeti(L: *mut lua_State, idx: c_int, n: lua_Integer) -> c_int;
    fn lua_seti(L: *mut lua_State, idx: c_int, n: lua_Integer);
    fn lua_geti(L: *mut lua_State, idx: c_int, n: lua_Integer) -> c_int;
    fn lua_rawlen(L: *mut lua_State, idx: c_int) -> usize;
    fn luaL_ref(L: *mut lua_State, t: c_int) -> c_int;
    fn luaL_unref(L: *mut lua_State, t: c_int, ref_: c_int);
    fn luaL_traceback(L: *mut lua_State, L1: *mut lua_State, msg: *const c_char, level: c_int);
}

fn lua_upvalueindex(i: c_int) -> c_int {
    LUA_REGISTRYINDEX - i
}

fn lua_pop(L: *mut lua_State, n: c_int) {
    unsafe {
        lua_settop(L, lua_gettop(L) - n);
    }
}

fn cstring(s: &str) -> CString {
    CString::new(s).unwrap_or_else(|_| CString::new(s.replace('\0', "")).unwrap())
}

#[derive(Debug, Clone, Deserialize)]
struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub version: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default = "default_entry")]
    pub entry: String,
}

impl PluginManifest {
    fn into_info(self) -> PluginManifestInfo {
        PluginManifestInfo {
            name: self.name,
            description: self.description,
            authors: self.authors,
            homepage: self.homepage,
            tags: self.tags,
            version: self.version,
            license: self.license,
            permissions: self.permissions,
            entry: self.entry,
        }
    }
}

fn default_entry() -> String {
    "init.lua".to_owned()
}

#[derive(Clone)]
struct RegisteredCommand {
    info: PluginCommandInfo,
    handler_ref: c_int,
}

struct PluginRuntime {
    manifest: PluginManifestInfo,
    root_dir: PathBuf,
    data_dir: PathBuf,
    plugin_idx: usize,
    vm: *mut lua_State,
    command_count: Arc<AtomicUsize>,
    commands: HashMap<String, RegisteredCommand>,
    aliases: HashMap<String, String>,
    callbacks: HashMap<PluginEventKind, Vec<c_int>>,
    windows: BTreeMap<String, PluginUiWindowSpec>,
    settings_pages: BTreeMap<String, PluginUiSettingsPageSpec>,
    host_panels: BTreeMap<String, PluginUiHostPanelSpec>,
}

unsafe impl Send for PluginRuntime {}
unsafe impl Sync for PluginRuntime {}

impl Drop for PluginRuntime {
    fn drop(&mut self) {
        unsafe {
            for cmd in self.commands.values() {
                if cmd.handler_ref != LUA_NOREF && cmd.handler_ref != LUA_REFNIL {
                    luaL_unref(self.vm, LUA_REGISTRYINDEX, cmd.handler_ref);
                }
            }
            for callbacks in self.callbacks.values_mut() {
                for callback in callbacks.drain(..) {
                    if callback != LUA_NOREF && callback != LUA_REFNIL {
                        luaL_unref(self.vm, LUA_REGISTRYINDEX, callback);
                    }
                }
            }
            lua_close(self.vm);
        }
    }
}

#[derive(Clone)]
struct PluginCommandIndex {
    plugin_idx: usize,
    canonical_name: String,
}

struct PluginLoadOutcome {
    status: PluginStatus,
}

pub struct LuaPluginHost {
    cmd_tx: mpsc::Sender<AppCommand>,
    plugins: RwLock<Vec<Arc<Mutex<PluginRuntime>>>>,
    statuses: RwLock<Vec<PluginStatus>>,
    command_index: RwLock<BTreeMap<String, PluginCommandIndex>>,
    auth: RwLock<PluginAuthSnapshot>,
    channels: RwLock<HashMap<ChannelId, PluginChannelSnapshot>>,
    current_channel: RwLock<Option<ChannelId>>,
    use_24h_timestamps: RwLock<bool>,
    plugin_root: PathBuf,
    session_started_unix_ms: i64,
}

impl LuaPluginHost {
    pub fn new(cmd_tx: mpsc::Sender<AppCommand>, use_24h_timestamps: bool) -> Arc<Self> {
        let host = Arc::new(Self {
            cmd_tx,
            plugins: RwLock::new(Vec::new()),
            statuses: RwLock::new(Vec::new()),
            command_index: RwLock::new(BTreeMap::new()),
            auth: RwLock::new(PluginAuthSnapshot::default()),
            channels: RwLock::new(HashMap::new()),
            current_channel: RwLock::new(None),
            use_24h_timestamps: RwLock::new(use_24h_timestamps),
            plugin_root: Self::plugin_root_dir(),
            session_started_unix_ms: system_time_unix_ms(),
        });
        set_global_host(Arc::clone(&host));
        host.reload();
        host
    }

    pub fn plugin_root_dir() -> PathBuf {
        ProjectDirs::from("dev", "crust", "crust")
            .map(|dirs| dirs.data_dir().join("plugins"))
            .unwrap_or_else(|| PathBuf::from("plugins"))
    }

    fn send_command(&self, cmd: AppCommand) {
        match self.cmd_tx.try_send(cmd) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(cmd)) => {
                if self.cmd_tx.blocking_send(cmd).is_err() {
                    warn!("plugin command channel closed");
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!("plugin command channel closed");
            }
        }
    }

    fn runtime_by_index(&self, idx: usize) -> Option<Arc<Mutex<PluginRuntime>>> {
        let guard = self.plugins.read().unwrap_or_else(|p| p.into_inner());
        guard.get(idx).cloned()
    }

    fn plugin_name_from_index(&self, idx: usize) -> Option<String> {
        self.runtime_by_index(idx).map(|plugin| {
            let guard = plugin.lock().unwrap_or_else(|p| p.into_inner());
            guard.manifest.name.clone()
        })
    }

    pub fn reload(&self) {
        let plugin_root = self.plugin_root.clone();
        if let Err(err) = std::fs::create_dir_all(&plugin_root) {
            warn!("plugins: failed to create plugin dir {plugin_root:?}: {err}");
            return;
        }

        {
            let mut guard = self.plugins.write().unwrap_or_else(|p| p.into_inner());
            guard.clear();
        }
        {
            let mut guard = plugin_state_index()
                .write()
                .unwrap_or_else(|p| p.into_inner());
            guard.clear();
        }
        {
            let mut guard = self.statuses.write().unwrap_or_else(|p| p.into_inner());
            guard.clear();
        }
        {
            let mut guard = self
                .command_index
                .write()
                .unwrap_or_else(|p| p.into_inner());
            guard.clear();
        }

        let mut statuses = Vec::new();
        let entries = match std::fs::read_dir(&plugin_root) {
            Ok(entries) => entries,
            Err(err) => {
                warn!("plugins: failed to read plugin dir {plugin_root:?}: {err}");
                return;
            }
        };

        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }

            let plugin_dir = entry.path();
            match self.load_plugin(plugin_dir.clone()) {
                Ok(outcome) => statuses.push(outcome.status),
                Err(err) => {
                    warn!("plugins: skipped {:?}: {err}", plugin_dir);
                    statuses.push(plugin_status_from_error(&plugin_dir, err));
                }
            }
        }

        let mut status_guard = self.statuses.write().unwrap_or_else(|p| p.into_inner());
        *status_guard = statuses;
        let count = self.plugins.read().unwrap_or_else(|p| p.into_inner()).len();
        info!("plugins: loaded {} plugin(s) from {:?}", count, plugin_root);
    }

    fn load_plugin(&self, dir: PathBuf) -> anyhow::Result<PluginLoadOutcome> {
        let manifest_path = dir.join("info.json");
        let manifest: PluginManifest =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)?;
        let data_dir = dir.join("data");
        std::fs::create_dir_all(&data_dir)?;

        let entry_path = dir.join(&manifest.entry);
        if !entry_path.exists() {
            anyhow::bail!("missing entry file {:?}", entry_path);
        }

        let vm = unsafe { luaL_newstate() };
        if vm.is_null() {
            anyhow::bail!("failed to create Lua VM");
        }

        unsafe {
            luaL_openselectedlibs(vm, -1, 0);
        }

        let runtime = PluginRuntime {
            manifest: manifest.clone().into_info(),
            root_dir: dir.clone(),
            data_dir,
            plugin_idx: 0,
            vm,
            command_count: Arc::new(AtomicUsize::new(0)),
            commands: HashMap::new(),
            aliases: HashMap::new(),
            callbacks: HashMap::new(),
            windows: BTreeMap::new(),
            settings_pages: BTreeMap::new(),
            host_panels: BTreeMap::new(),
        };
        let runtime = Arc::new(Mutex::new(runtime));

        let plugin_idx = {
            let mut guard = self.plugins.write().unwrap_or_else(|p| p.into_inner());
            let idx = guard.len();
            guard.push(Arc::clone(&runtime));
            idx
        };

        {
            let mut guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
            guard.plugin_idx = plugin_idx;
        }
        {
            let vm_key = vm as usize;
            let mut guard = plugin_state_index()
                .write()
                .unwrap_or_else(|p| p.into_inner());
            guard.insert(vm_key, plugin_idx);
        }

        self.register_globals_for_plugin(plugin_idx, &runtime);

        let entry_c = CString::new(entry_path.to_string_lossy().as_bytes())?;
        let run_rc = unsafe { luaL_loadfilex(vm, entry_c.as_ptr(), cstring("t").as_ptr()) };
        if run_rc != LUA_OK {
            let err = unsafe { lua_error_text(vm, run_rc) };
            self.discard_failed_plugin(plugin_idx);
            anyhow::bail!("failed to load {:?}: {err}", entry_path);
        }

        let run_rc = unsafe { lua_pcallk(vm, 0, LUA_MULTRET, 0, 0, None) };
        if run_rc != LUA_OK {
            let err = unsafe { lua_error_text(vm, run_rc) };
            self.discard_failed_plugin(plugin_idx);
            anyhow::bail!("failed to run {:?}: {err}", entry_path);
        }

        let command_count = {
            let guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
            guard.command_count.load(Ordering::SeqCst)
        };

        Ok(PluginLoadOutcome {
            status: PluginStatus {
                manifest: manifest.into_info(),
                loaded: true,
                command_count,
                error: None,
            },
        })
    }

    fn discard_failed_plugin(&self, plugin_idx: usize) {
        {
            let mut index = self
                .command_index
                .write()
                .unwrap_or_else(|p| p.into_inner());
            index.retain(|_, entry| entry.plugin_idx != plugin_idx);
        }
        let mut plugins = self.plugins.write().unwrap_or_else(|p| p.into_inner());
        let _ = plugins.pop();
    }

    fn register_globals_for_plugin(&self, plugin_idx: usize, runtime: &Arc<Mutex<PluginRuntime>>) {
        let vm = {
            let guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
            guard.vm
        };
        unsafe {
            lua_createtable(vm, 0, 0);
            let c2_index = lua_absindex(vm, -1);

            lua_createtable(vm, 0, 0);
            set_field_int(vm, -1, "Debug", 0);
            set_field_int(vm, -1, "Info", 1);
            set_field_int(vm, -1, "Warning", 2);
            set_field_int(vm, -1, "Critical", 3);
            lua_setfield(vm, -2, cstring("LogLevel").as_ptr());

            push_event_type_table(vm);
            lua_setfield(vm, -2, cstring("EventType").as_ptr());

            register_c2_fn(vm, c2_index, "log", native_log, plugin_idx);
            register_c2_fn(
                vm,
                c2_index,
                "register_command",
                native_register_command,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "register_callback",
                native_register_callback,
                plugin_idx,
            );
            register_c2_fn(vm, c2_index, "later", native_later, plugin_idx);
            register_c2_fn(
                vm,
                c2_index,
                "current_account",
                native_current_account,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "channel_by_name",
                native_channel_by_name,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "current_channel",
                native_current_channel,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "join_channel",
                native_join_channel,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "join_irc_channel",
                native_join_irc_channel,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "leave_channel",
                native_leave_channel,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "send_message",
                native_send_message,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "send_whisper",
                native_send_whisper,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "add_system_message",
                native_add_system_message,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "clear_messages",
                native_clear_messages,
                plugin_idx,
            );
            register_c2_fn(vm, c2_index, "open_url", native_open_url, plugin_idx);
            register_c2_fn(
                vm,
                c2_index,
                "show_user_card",
                native_show_user_card,
                plugin_idx,
            );
            register_c2_fn(vm, c2_index, "fetch_image", native_fetch_image, plugin_idx);
            register_c2_fn(
                vm,
                c2_index,
                "fetch_link_preview",
                native_fetch_link_preview,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "load_channel_emotes",
                native_load_channel_emotes,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "fetch_stream_status",
                native_fetch_stream_status,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "fetch_user_profile",
                native_fetch_user_profile,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "fetch_ivr_logs",
                native_fetch_ivr_logs,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "load_older_local_history",
                native_load_older_local_history,
                plugin_idx,
            );
            register_c2_fn(vm, c2_index, "login", native_login, plugin_idx);
            register_c2_fn(vm, c2_index, "logout", native_logout, plugin_idx);
            register_c2_fn(vm, c2_index, "add_account", native_add_account, plugin_idx);
            register_c2_fn(
                vm,
                c2_index,
                "switch_account",
                native_switch_account,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "remove_account",
                native_remove_account,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "set_default_account",
                native_set_default_account,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "refresh_auth",
                native_refresh_auth,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "set_irc_nick",
                native_set_irc_nick,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "set_irc_auth",
                native_set_irc_auth,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "set_beta_features",
                native_set_beta_features,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "set_always_on_top",
                native_set_always_on_top,
                plugin_idx,
            );
            register_c2_fn(vm, c2_index, "set_theme", native_set_theme, plugin_idx);
            register_c2_fn(
                vm,
                c2_index,
                "set_chat_ui_behavior",
                native_set_chat_ui_behavior,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "set_general_settings",
                native_set_general_settings,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "set_slash_usage_counts",
                native_set_slash_usage_counts,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "set_emote_picker_preferences",
                native_set_emote_picker_preferences,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "set_appearance_settings",
                native_set_appearance_settings,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "set_highlight_rules",
                native_set_highlight_rules,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "set_filter_records",
                native_set_filter_records,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "set_mod_action_presets",
                native_set_mod_action_presets,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "set_notification_settings",
                native_set_notification_settings,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "reload_plugins",
                native_reload_plugins,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "timeout_user",
                native_timeout_user,
                plugin_idx,
            );
            register_c2_fn(vm, c2_index, "ban_user", native_ban_user, plugin_idx);
            register_c2_fn(vm, c2_index, "unban_user", native_unban_user, plugin_idx);
            register_c2_fn(vm, c2_index, "warn_user", native_warn_user, plugin_idx);
            register_c2_fn(
                vm,
                c2_index,
                "set_suspicious_user",
                native_set_suspicious_user,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "clear_suspicious_user",
                native_clear_suspicious_user,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "resolve_automod_message",
                native_resolve_automod_message,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "fetch_unban_requests",
                native_fetch_unban_requests,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "resolve_unban_request",
                native_resolve_unban_request,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "open_moderation_tools",
                native_open_moderation_tools,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "update_reward_redemption_status",
                native_update_reward_redemption_status,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "delete_message",
                native_delete_message,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "clear_user_messages_locally",
                native_clear_user_messages_locally,
                plugin_idx,
            );
            register_c2_fn(vm, c2_index, "create_poll", native_create_poll, plugin_idx);
            register_c2_fn(vm, c2_index, "end_poll", native_end_poll, plugin_idx);
            register_c2_fn(
                vm,
                c2_index,
                "create_prediction",
                native_create_prediction,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "lock_prediction",
                native_lock_prediction,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "resolve_prediction",
                native_resolve_prediction,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "cancel_prediction",
                native_cancel_prediction,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "start_commercial",
                native_start_commercial,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "create_stream_marker",
                native_create_stream_marker,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "send_announcement",
                native_send_announcement,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "send_shoutout",
                native_send_shoutout,
                plugin_idx,
            );
            register_c2_fn(vm, c2_index, "plugin_dir", native_plugin_dir, plugin_idx);
            register_c2_fn(
                vm,
                c2_index,
                "plugin_data_dir",
                native_plugin_data_dir,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "use_24h_timestamps",
                native_use_24h_timestamps,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                c2_index,
                "session_started_ms",
                native_session_started_ms,
                plugin_idx,
            );
            lua_createtable(vm, 0, 0);
            let ui_index = lua_absindex(vm, -1);
            register_c2_fn(
                vm,
                ui_index,
                "register_window",
                native_ui_register_window,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                ui_index,
                "update_window",
                native_ui_update_window,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                ui_index,
                "open_window",
                native_ui_open_window,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                ui_index,
                "close_window",
                native_ui_close_window,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                ui_index,
                "unregister_window",
                native_ui_unregister_window,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                ui_index,
                "register_settings_page",
                native_ui_register_settings_page,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                ui_index,
                "update_settings_page",
                native_ui_update_settings_page,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                ui_index,
                "unregister_settings_page",
                native_ui_unregister_settings_page,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                ui_index,
                "register_host_panel",
                native_ui_register_host_panel,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                ui_index,
                "update_host_panel",
                native_ui_update_host_panel,
                plugin_idx,
            );
            register_c2_fn(
                vm,
                ui_index,
                "unregister_host_panel",
                native_ui_unregister_host_panel,
                plugin_idx,
            );
            lua_setfield(vm, c2_index, cstring("ui").as_ptr());
            lua_setglobal(vm, cstring("c2").as_ptr());

            lua_pushinteger(vm, plugin_idx as lua_Integer);
            lua_pushcclosure(vm, Some(native_print), 1);
            lua_setglobal(vm, cstring("print").as_ptr());

            configure_package_path(
                vm,
                &runtime.lock().unwrap_or_else(|p| p.into_inner()).root_dir,
            );
        }
    }

    fn plugin_snapshot_for(&self, channel: &ChannelId) -> PluginChannelSnapshot {
        self.channels
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(channel)
            .cloned()
            .unwrap_or_default()
    }

    fn current_auth_snapshot(&self) -> PluginAuthSnapshot {
        self.auth.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn current_channel_snapshot(&self) -> Option<ChannelId> {
        self.current_channel
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn session_started_unix_ms(&self) -> i64 {
        self.session_started_unix_ms
    }

    fn plugin_ui_snapshot_inner(&self) -> PluginUiSnapshot {
        let plugins: Vec<_> = self
            .plugins
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned()
            .collect();
        let mut snapshot = PluginUiSnapshot::default();
        for plugin in plugins {
            let guard = plugin.lock().unwrap_or_else(|p| p.into_inner());
            for window in guard.windows.values() {
                snapshot.windows.push(PluginUiWindowRegistration {
                    plugin_name: guard.manifest.name.clone(),
                    window: window.clone(),
                });
            }
            for page in guard.settings_pages.values() {
                snapshot
                    .settings_pages
                    .push(PluginUiSettingsPageRegistration {
                        plugin_name: guard.manifest.name.clone(),
                        page: page.clone(),
                    });
            }
            for panel in guard.host_panels.values() {
                snapshot.host_panels.push(PluginUiHostPanelRegistration {
                    plugin_name: guard.manifest.name.clone(),
                    panel: panel.clone(),
                });
            }
        }
        snapshot
    }

    fn set_plugin_window_open_inner(&self, plugin_name: &str, window_id: &str, open: bool) {
        let plugins: Vec<_> = self
            .plugins
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned()
            .collect();
        for plugin in plugins {
            let mut guard = plugin.lock().unwrap_or_else(|p| p.into_inner());
            if guard.manifest.name != plugin_name {
                continue;
            }
            if let Some(window) = guard.windows.get_mut(window_id) {
                window.open = open;
            }
            break;
        }
    }

    fn set_plugin_window_spec(&self, plugin_idx: usize, spec: PluginUiWindowSpec) {
        if let Some(runtime) = self.runtime_by_index(plugin_idx) {
            let mut guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
            guard.windows.insert(spec.id.clone(), spec);
        }
    }

    fn set_plugin_settings_page_spec(&self, plugin_idx: usize, spec: PluginUiSettingsPageSpec) {
        if let Some(runtime) = self.runtime_by_index(plugin_idx) {
            let mut guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
            guard.settings_pages.insert(spec.id.clone(), spec);
        }
    }

    fn set_plugin_host_panel_spec(&self, plugin_idx: usize, spec: PluginUiHostPanelSpec) {
        if let Some(runtime) = self.runtime_by_index(plugin_idx) {
            let mut guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
            guard.host_panels.insert(spec.id.clone(), spec);
        }
    }

    fn remove_plugin_window(&self, plugin_idx: usize, id: &str) {
        if let Some(runtime) = self.runtime_by_index(plugin_idx) {
            let mut guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
            guard.windows.remove(id);
        }
    }

    fn remove_plugin_settings_page(&self, plugin_idx: usize, id: &str) {
        if let Some(runtime) = self.runtime_by_index(plugin_idx) {
            let mut guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
            guard.settings_pages.remove(id);
        }
    }

    fn remove_plugin_host_panel(&self, plugin_idx: usize, id: &str) {
        if let Some(runtime) = self.runtime_by_index(plugin_idx) {
            let mut guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
            guard.host_panels.remove(id);
        }
    }

    fn register_callback_ref(&self, plugin_idx: usize, kind: PluginEventKind, callback_ref: c_int) {
        if let Some(runtime) = self.runtime_by_index(plugin_idx) {
            let mut guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
            guard.callbacks.entry(kind).or_default().push(callback_ref);
        }
    }

    fn callbacks_for(&self, plugin_idx: usize, kind: PluginEventKind) -> Vec<c_int> {
        self.runtime_by_index(plugin_idx)
            .and_then(|runtime| {
                let guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
                guard.callbacks.get(&kind).cloned()
            })
            .unwrap_or_default()
    }

    fn dispatch_event_inner(&self, event: &AppEvent) {
        let plugins: Vec<_> = self
            .plugins
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned()
            .collect();

        let Some(kind) = event_kind(event) else {
            return;
        };
        let target_plugin_name = plugin_target_name(event);

        for plugin in plugins {
            let (vm, callbacks, plugin_name) = {
                let guard = plugin.lock().unwrap_or_else(|p| p.into_inner());
                (
                    guard.vm,
                    guard.callbacks.get(&kind).cloned().unwrap_or_default(),
                    guard.manifest.name.clone(),
                )
            };
            if callbacks.is_empty() {
                continue;
            }
            if let Some(target_plugin_name) = target_plugin_name.as_deref() {
                if plugin_name != target_plugin_name {
                    continue;
                }
            }

            for callback in callbacks {
                let arg = unsafe { make_event_table(vm, event) };
                let result = unsafe { call_event_callback(vm, callback, arg) };
                unsafe {
                    lua_pop(vm, 1);
                }
                if let Err(err) = result {
                    warn!("plugins: event callback in {} failed: {err}", plugin_name);
                }
            }
        }
    }
}

impl PluginHost for LuaPluginHost {
    fn plugin_statuses(&self) -> Vec<PluginStatus> {
        self.statuses
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn plugin_ui_snapshot(&self) -> PluginUiSnapshot {
        self.plugin_ui_snapshot_inner()
    }

    fn command_infos(&self) -> Vec<PluginCommandInfo> {
        let guard = self.plugins.read().unwrap_or_else(|p| p.into_inner());
        let mut out = Vec::new();
        for plugin in guard.iter() {
            let plugin = plugin.lock().unwrap_or_else(|p| p.into_inner());
            for cmd in plugin.commands.values() {
                out.push(cmd.info.clone());
            }
        }
        out
    }

    fn complete_command(&self, request: PluginCompletionRequest) -> PluginCompletionList {
        let plugins: Vec<_> = self
            .plugins
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned()
            .collect();
        let mut merged = PluginCompletionList::default();

        for plugin in plugins {
            let (vm, callbacks, plugin_name) = {
                let guard = plugin.lock().unwrap_or_else(|p| p.into_inner());
                (
                    guard.vm,
                    guard
                        .callbacks
                        .get(&PluginEventKind::CompletionRequested)
                        .cloned()
                        .unwrap_or_default(),
                    guard.manifest.name.clone(),
                )
            };
            for callback in callbacks {
                let event = make_completion_event(vm, &request, self);
                let result = unsafe { call_completion_callback(vm, callback, event) };
                unsafe {
                    lua_pop(vm, 1);
                }
                match result {
                    Ok(Some(list)) => {
                        if list.hide_others {
                            return list;
                        }
                        merged.values.extend(list.values);
                    }
                    Ok(None) => {}
                    Err(err) => {
                        warn!(
                            "plugins: completion callback in {} failed: {err}",
                            plugin_name
                        );
                    }
                }
            }
        }

        merged.values.sort();
        merged.values.dedup();
        merged
    }

    fn execute_command(&self, invocation: PluginCommandInvocation) {
        let key = normalize_command_name(&invocation.command);
        let index = {
            let guard = self.command_index.read().unwrap_or_else(|p| p.into_inner());
            guard.get(&key).cloned()
        };
        let Some(index) = index else {
            self.send_command(AppCommand::InjectLocalMessage {
                channel: invocation.channel,
                text: format!("Unknown plugin command: /{}", invocation.command),
            });
            return;
        };

        let Some(plugin) = self.runtime_by_index(index.plugin_idx) else {
            self.send_command(AppCommand::InjectLocalMessage {
                channel: invocation.channel,
                text: format!("Plugin command /{} is unavailable", invocation.command),
            });
            return;
        };

        let (vm, handler_ref, plugin_name) = {
            let guard = plugin.lock().unwrap_or_else(|p| p.into_inner());
            let Some(cmd) = guard.commands.get(&index.canonical_name) else {
                self.send_command(AppCommand::InjectLocalMessage {
                    channel: invocation.channel,
                    text: format!("Plugin command /{} is unavailable", invocation.command),
                });
                return;
            };
            (guard.vm, cmd.handler_ref, guard.manifest.name.clone())
        };

        let ctx = make_command_context(vm, self, &invocation);
        let result = unsafe { call_command_handler(vm, handler_ref, ctx) };
        unsafe {
            lua_pop(vm, 1);
        }

        match result {
            Ok(Some(text)) if !text.trim().is_empty() => {
                self.send_command(AppCommand::InjectLocalMessage {
                    channel: invocation.channel,
                    text,
                });
            }
            Ok(_) => {}
            Err(err) => {
                self.send_command(AppCommand::InjectLocalMessage {
                    channel: invocation.channel,
                    text: format!("[plugin:{}] {}", plugin_name, err),
                });
            }
        }
    }

    fn dispatch_event(&self, event: &AppEvent) {
        self.dispatch_event_inner(event);
    }

    fn run_plugin_callback(&self, vm_key: usize, callback_ref: i32) {
        self.run_plugin_callback_inner(vm_key, callback_ref);
    }

    fn use_24h_timestamps(&self) -> bool {
        *self
            .use_24h_timestamps
            .read()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn set_use_24h_timestamps(&self, enabled: bool) {
        *self
            .use_24h_timestamps
            .write()
            .unwrap_or_else(|p| p.into_inner()) = enabled;
    }

    fn reload(&self) {
        LuaPluginHost::reload(self);
    }

    fn update_auth_snapshot(&self, snapshot: PluginAuthSnapshot) {
        *self.auth.write().unwrap_or_else(|p| p.into_inner()) = snapshot;
    }

    fn update_channel_snapshot(&self, channel: ChannelId, snapshot: PluginChannelSnapshot) {
        self.channels
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(channel, snapshot);
    }

    fn set_current_channel(&self, channel: Option<ChannelId>) {
        *self
            .current_channel
            .write()
            .unwrap_or_else(|p| p.into_inner()) = channel;
    }

    fn set_plugin_window_open(&self, plugin_name: &str, window_id: &str, open: bool) {
        self.set_plugin_window_open_inner(plugin_name, window_id, open);
    }
}

fn plugin_status_from_error(dir: &Path, err: anyhow::Error) -> PluginStatus {
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown-plugin");
    PluginStatus {
        manifest: PluginManifestInfo {
            name: name.to_owned(),
            ..PluginManifestInfo::default()
        },
        loaded: false,
        command_count: 0,
        error: Some(err.to_string()),
    }
}

fn system_time_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|dur| dur.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn event_kind_name(kind: PluginEventKind) -> &'static str {
    match kind {
        PluginEventKind::CompletionRequested => "CompletionRequested",
        PluginEventKind::EmoteImageReady => "EmoteImageReady",
        PluginEventKind::Authenticated => "Authenticated",
        PluginEventKind::EmoteCatalogUpdated => "EmoteCatalogUpdated",
        PluginEventKind::LoggedOut => "LoggedOut",
        PluginEventKind::AccountListUpdated => "AccountListUpdated",
        PluginEventKind::ChannelJoined => "ChannelJoined",
        PluginEventKind::ChannelParted => "ChannelParted",
        PluginEventKind::ChannelRedirected => "ChannelRedirected",
        PluginEventKind::ConnectionStateChanged => "ConnectionStateChanged",
        PluginEventKind::MessageReceived => "MessageReceived",
        PluginEventKind::WhisperReceived => "WhisperReceived",
        PluginEventKind::MessageDeleted => "MessageDeleted",
        PluginEventKind::SystemNotice => "SystemNotice",
        PluginEventKind::Error => "Error",
        PluginEventKind::HistoryLoaded => "HistoryLoaded",
        PluginEventKind::UserProfileLoaded => "UserProfileLoaded",
        PluginEventKind::UserProfileUnavailable => "UserProfileUnavailable",
        PluginEventKind::StreamStatusUpdated => "StreamStatusUpdated",
        PluginEventKind::IvrLogsLoaded => "IvrLogsLoaded",
        PluginEventKind::IvrLogsFailed => "IvrLogsFailed",
        PluginEventKind::ChannelEmotesLoaded => "ChannelEmotesLoaded",
        PluginEventKind::BetaFeaturesUpdated => "BetaFeaturesUpdated",
        PluginEventKind::ChatUiBehaviorUpdated => "ChatUiBehaviorUpdated",
        PluginEventKind::GeneralSettingsUpdated => "GeneralSettingsUpdated",
        PluginEventKind::SlashUsageCountsUpdated => "SlashUsageCountsUpdated",
        PluginEventKind::EmotePickerPreferencesUpdated => "EmotePickerPreferencesUpdated",
        PluginEventKind::AppearanceSettingsUpdated => "AppearanceSettingsUpdated",
        PluginEventKind::FontSettingsUpdated => "FontSettingsUpdated",
        PluginEventKind::RestoreLastActiveChannel => "RestoreLastActiveChannel",
        PluginEventKind::RoomStateUpdated => "RoomStateUpdated",
        PluginEventKind::AutoModQueueAppend => "AutoModQueueAppend",
        PluginEventKind::AutoModQueueRemove => "AutoModQueueRemove",
        PluginEventKind::UnbanRequestsLoaded => "UnbanRequestsLoaded",
        PluginEventKind::UnbanRequestsFailed => "UnbanRequestsFailed",
        PluginEventKind::UnbanRequestUpsert => "UnbanRequestUpsert",
        PluginEventKind::UnbanRequestResolved => "UnbanRequestResolved",
        PluginEventKind::OpenModerationTools => "OpenModerationTools",
        PluginEventKind::HighlightRulesUpdated => "HighlightRulesUpdated",
        PluginEventKind::FilterRecordsUpdated => "FilterRecordsUpdated",
        PluginEventKind::ModActionPresetsUpdated => "ModActionPresetsUpdated",
        PluginEventKind::NicknamesUpdated => "NicknamesUpdated",
        PluginEventKind::IgnoredUsersUpdated => "IgnoredUsersUpdated",
        PluginEventKind::IgnoredPhrasesUpdated => "IgnoredPhrasesUpdated",
        PluginEventKind::UserPronounsLoaded => "UserPronounsLoaded",
        PluginEventKind::UsercardSettingsUpdated => "UsercardSettingsUpdated",
        PluginEventKind::SelfAvatarLoaded => "SelfAvatarLoaded",
        PluginEventKind::LinkPreviewReady => "LinkPreviewReady",
        PluginEventKind::SenderCosmeticsUpdated => "SenderCosmeticsUpdated",
        PluginEventKind::IrcTopicChanged => "IrcTopicChanged",
        PluginEventKind::AuthExpired => "AuthExpired",
        PluginEventKind::UserMessagesCleared => "UserMessagesCleared",
        PluginEventKind::LowTrustStatusUpdated => "LowTrustStatusUpdated",
        PluginEventKind::UserStateUpdated => "UserStateUpdated",
        PluginEventKind::ChannelMessagesCleared => "ChannelMessagesCleared",
        PluginEventKind::ClearUserMessagesLocally => "ClearUserMessagesLocally",
        PluginEventKind::ImagePrefetchQueued => "ImagePrefetchQueued",
        PluginEventKind::PluginUiAction => "PluginUiAction",
        PluginEventKind::PluginUiChange => "PluginUiChange",
        PluginEventKind::PluginUiSubmit => "PluginUiSubmit",
        PluginEventKind::PluginUiWindowClosed => "PluginUiWindowClosed",
    }
}

fn push_event_type_table(L: *mut lua_State) {
    unsafe {
        lua_createtable(L, 0, 0);
        let kinds = [
            PluginEventKind::CompletionRequested,
            PluginEventKind::EmoteImageReady,
            PluginEventKind::Authenticated,
            PluginEventKind::EmoteCatalogUpdated,
            PluginEventKind::LoggedOut,
            PluginEventKind::AccountListUpdated,
            PluginEventKind::ChannelJoined,
            PluginEventKind::ChannelParted,
            PluginEventKind::ChannelRedirected,
            PluginEventKind::ConnectionStateChanged,
            PluginEventKind::MessageReceived,
            PluginEventKind::WhisperReceived,
            PluginEventKind::MessageDeleted,
            PluginEventKind::SystemNotice,
            PluginEventKind::Error,
            PluginEventKind::HistoryLoaded,
            PluginEventKind::UserProfileLoaded,
            PluginEventKind::UserProfileUnavailable,
            PluginEventKind::StreamStatusUpdated,
            PluginEventKind::IvrLogsLoaded,
            PluginEventKind::IvrLogsFailed,
            PluginEventKind::ChannelEmotesLoaded,
            PluginEventKind::BetaFeaturesUpdated,
            PluginEventKind::ChatUiBehaviorUpdated,
            PluginEventKind::GeneralSettingsUpdated,
            PluginEventKind::SlashUsageCountsUpdated,
            PluginEventKind::EmotePickerPreferencesUpdated,
            PluginEventKind::AppearanceSettingsUpdated,
            PluginEventKind::FontSettingsUpdated,
            PluginEventKind::RestoreLastActiveChannel,
            PluginEventKind::RoomStateUpdated,
            PluginEventKind::AutoModQueueAppend,
            PluginEventKind::AutoModQueueRemove,
            PluginEventKind::UnbanRequestsLoaded,
            PluginEventKind::UnbanRequestsFailed,
            PluginEventKind::UnbanRequestUpsert,
            PluginEventKind::UnbanRequestResolved,
            PluginEventKind::OpenModerationTools,
            PluginEventKind::HighlightRulesUpdated,
            PluginEventKind::FilterRecordsUpdated,
            PluginEventKind::ModActionPresetsUpdated,
            PluginEventKind::SelfAvatarLoaded,
            PluginEventKind::LinkPreviewReady,
            PluginEventKind::SenderCosmeticsUpdated,
            PluginEventKind::IrcTopicChanged,
            PluginEventKind::AuthExpired,
            PluginEventKind::UserMessagesCleared,
            PluginEventKind::LowTrustStatusUpdated,
            PluginEventKind::UserStateUpdated,
            PluginEventKind::ChannelMessagesCleared,
            PluginEventKind::ClearUserMessagesLocally,
            PluginEventKind::ImagePrefetchQueued,
            PluginEventKind::PluginUiAction,
            PluginEventKind::PluginUiChange,
            PluginEventKind::PluginUiSubmit,
            PluginEventKind::PluginUiWindowClosed,
            PluginEventKind::NicknamesUpdated,
            PluginEventKind::IgnoredUsersUpdated,
            PluginEventKind::IgnoredPhrasesUpdated,
            PluginEventKind::UserPronounsLoaded,
            PluginEventKind::UsercardSettingsUpdated,
        ];
        for (idx, kind) in kinds.into_iter().enumerate() {
            set_field_int(L, -1, event_kind_name(kind), idx as i64);
        }
    }
}

fn event_kind_from_value(L: *mut lua_State, idx: c_int) -> Option<PluginEventKind> {
    unsafe {
        match lua_type(L, idx) {
            LUA_TNUMBER => match lua_value_int(L, idx)? {
                0 => Some(PluginEventKind::CompletionRequested),
                1 => Some(PluginEventKind::EmoteImageReady),
                2 => Some(PluginEventKind::Authenticated),
                3 => Some(PluginEventKind::EmoteCatalogUpdated),
                4 => Some(PluginEventKind::LoggedOut),
                5 => Some(PluginEventKind::AccountListUpdated),
                6 => Some(PluginEventKind::ChannelJoined),
                7 => Some(PluginEventKind::ChannelParted),
                8 => Some(PluginEventKind::ChannelRedirected),
                9 => Some(PluginEventKind::ConnectionStateChanged),
                10 => Some(PluginEventKind::MessageReceived),
                11 => Some(PluginEventKind::WhisperReceived),
                12 => Some(PluginEventKind::MessageDeleted),
                13 => Some(PluginEventKind::SystemNotice),
                14 => Some(PluginEventKind::Error),
                15 => Some(PluginEventKind::HistoryLoaded),
                16 => Some(PluginEventKind::UserProfileLoaded),
                17 => Some(PluginEventKind::UserProfileUnavailable),
                18 => Some(PluginEventKind::StreamStatusUpdated),
                19 => Some(PluginEventKind::IvrLogsLoaded),
                20 => Some(PluginEventKind::IvrLogsFailed),
                21 => Some(PluginEventKind::ChannelEmotesLoaded),
                22 => Some(PluginEventKind::BetaFeaturesUpdated),
                23 => Some(PluginEventKind::ChatUiBehaviorUpdated),
                24 => Some(PluginEventKind::GeneralSettingsUpdated),
                25 => Some(PluginEventKind::SlashUsageCountsUpdated),
                26 => Some(PluginEventKind::EmotePickerPreferencesUpdated),
                27 => Some(PluginEventKind::AppearanceSettingsUpdated),
                28 => Some(PluginEventKind::RoomStateUpdated),
                29 => Some(PluginEventKind::AutoModQueueAppend),
                30 => Some(PluginEventKind::AutoModQueueRemove),
                31 => Some(PluginEventKind::UnbanRequestsLoaded),
                32 => Some(PluginEventKind::UnbanRequestsFailed),
                33 => Some(PluginEventKind::UnbanRequestUpsert),
                34 => Some(PluginEventKind::UnbanRequestResolved),
                35 => Some(PluginEventKind::OpenModerationTools),
                36 => Some(PluginEventKind::HighlightRulesUpdated),
                37 => Some(PluginEventKind::FilterRecordsUpdated),
                38 => Some(PluginEventKind::ModActionPresetsUpdated),
                39 => Some(PluginEventKind::SelfAvatarLoaded),
                40 => Some(PluginEventKind::LinkPreviewReady),
                41 => Some(PluginEventKind::SenderCosmeticsUpdated),
                42 => Some(PluginEventKind::IrcTopicChanged),
                43 => Some(PluginEventKind::AuthExpired),
                44 => Some(PluginEventKind::UserMessagesCleared),
                45 => Some(PluginEventKind::UserStateUpdated),
                46 => Some(PluginEventKind::ChannelMessagesCleared),
                47 => Some(PluginEventKind::ClearUserMessagesLocally),
                48 => Some(PluginEventKind::ImagePrefetchQueued),
                49 => Some(PluginEventKind::PluginUiAction),
                50 => Some(PluginEventKind::PluginUiChange),
                51 => Some(PluginEventKind::PluginUiSubmit),
                52 => Some(PluginEventKind::PluginUiWindowClosed),
                53 => Some(PluginEventKind::NicknamesUpdated),
                54 => Some(PluginEventKind::IgnoredUsersUpdated),
                55 => Some(PluginEventKind::IgnoredPhrasesUpdated),
                56 => Some(PluginEventKind::UserPronounsLoaded),
                57 => Some(PluginEventKind::UsercardSettingsUpdated),
                58 => Some(PluginEventKind::FontSettingsUpdated),
                59 => Some(PluginEventKind::RestoreLastActiveChannel),
                60 => Some(PluginEventKind::LowTrustStatusUpdated),
                _ => None,
            },
            LUA_TSTRING => {
                let name = lua_value_string(L, idx)?.to_ascii_lowercase();
                Some(match name.as_str() {
                    "completionrequested" => PluginEventKind::CompletionRequested,
                    "emoteimageready" => PluginEventKind::EmoteImageReady,
                    "authenticated" => PluginEventKind::Authenticated,
                    "emotecatalogupdated" => PluginEventKind::EmoteCatalogUpdated,
                    "loggedout" => PluginEventKind::LoggedOut,
                    "accountlistupdated" => PluginEventKind::AccountListUpdated,
                    "channeljoined" => PluginEventKind::ChannelJoined,
                    "channelparted" => PluginEventKind::ChannelParted,
                    "channelredirected" => PluginEventKind::ChannelRedirected,
                    "connectionstatechanged" => PluginEventKind::ConnectionStateChanged,
                    "messagereceived" => PluginEventKind::MessageReceived,
                    "whisperreceived" => PluginEventKind::WhisperReceived,
                    "messagedeleted" => PluginEventKind::MessageDeleted,
                    "systemnotice" => PluginEventKind::SystemNotice,
                    "error" => PluginEventKind::Error,
                    "historyloaded" => PluginEventKind::HistoryLoaded,
                    "userprofileloaded" => PluginEventKind::UserProfileLoaded,
                    "userprofileunavailable" => PluginEventKind::UserProfileUnavailable,
                    "streamstatusupdated" => PluginEventKind::StreamStatusUpdated,
                    "ivrlogsloaded" => PluginEventKind::IvrLogsLoaded,
                    "ivrlogsfailed" => PluginEventKind::IvrLogsFailed,
                    "channelemotesloaded" => PluginEventKind::ChannelEmotesLoaded,
                    "betafeaturesupdated" => PluginEventKind::BetaFeaturesUpdated,
                    "chatuibehaviorupdated" => PluginEventKind::ChatUiBehaviorUpdated,
                    "generalsettingsupdated" => PluginEventKind::GeneralSettingsUpdated,
                    "slashusagecountsupdated" => PluginEventKind::SlashUsageCountsUpdated,
                    "emotepickerpreferencesupdated" => {
                        PluginEventKind::EmotePickerPreferencesUpdated
                    }
                    "appearancesettingsupdated" => PluginEventKind::AppearanceSettingsUpdated,
                    "roomstateupdated" => PluginEventKind::RoomStateUpdated,
                    "automodqueueappend" => PluginEventKind::AutoModQueueAppend,
                    "automodqueueremove" => PluginEventKind::AutoModQueueRemove,
                    "unbanrequestsloaded" => PluginEventKind::UnbanRequestsLoaded,
                    "unbanrequestsfailed" => PluginEventKind::UnbanRequestsFailed,
                    "unbanrequestupsert" => PluginEventKind::UnbanRequestUpsert,
                    "unbanrequestresolved" => PluginEventKind::UnbanRequestResolved,
                    "openmoderationtools" => PluginEventKind::OpenModerationTools,
                    "highlightrulesupdated" => PluginEventKind::HighlightRulesUpdated,
                    "filterrecordsupdated" => PluginEventKind::FilterRecordsUpdated,
                    "modactionpresetsupdated" => PluginEventKind::ModActionPresetsUpdated,
                    "selfavatarloaded" => PluginEventKind::SelfAvatarLoaded,
                    "linkpreviewready" => PluginEventKind::LinkPreviewReady,
                    "sendercosmeticsupdated" => PluginEventKind::SenderCosmeticsUpdated,
                    "irctopicchanged" => PluginEventKind::IrcTopicChanged,
                    "authexpired" => PluginEventKind::AuthExpired,
                    "usermessagescleared" => PluginEventKind::UserMessagesCleared,
                    "lowtruststatusupdated" => PluginEventKind::LowTrustStatusUpdated,
                    "userstateupdated" => PluginEventKind::UserStateUpdated,
                    "channelmessagescleared" => PluginEventKind::ChannelMessagesCleared,
                    "clearusermessageslocally" => PluginEventKind::ClearUserMessagesLocally,
                    "imageprefetchqueued" => PluginEventKind::ImagePrefetchQueued,
                    "pluginuiaction" => PluginEventKind::PluginUiAction,
                    "pluginuichange" => PluginEventKind::PluginUiChange,
                    "pluginuisubmit" => PluginEventKind::PluginUiSubmit,
                    "pluginuiwindowclosed" => PluginEventKind::PluginUiWindowClosed,
                    "nicknamesupdated" => PluginEventKind::NicknamesUpdated,
                    "ignoredusersupdated" => PluginEventKind::IgnoredUsersUpdated,
                    "ignoredphrasesupdated" => PluginEventKind::IgnoredPhrasesUpdated,
                    "userpronounsloaded" => PluginEventKind::UserPronounsLoaded,
                    "usercardsettingsupdated" => PluginEventKind::UsercardSettingsUpdated,
                    "fontsettingsupdated" => PluginEventKind::FontSettingsUpdated,
                    "restorelastactivechannel" => PluginEventKind::RestoreLastActiveChannel,
                    _ => return None,
                })
            }
            _ => None,
        }
    }
}

unsafe fn register_c2_fn(
    L: *mut lua_State,
    table_index: c_int,
    name: &str,
    func: unsafe extern "C" fn(*mut lua_State) -> c_int,
    plugin_idx: usize,
) {
    let table_index = lua_absindex(L, table_index);
    lua_pushinteger(L, plugin_idx as lua_Integer);
    lua_pushcclosure(L, Some(func), 1);
    lua_setfield(L, table_index, cstring(name).as_ptr());
}

unsafe extern "C" fn native_print(L: *mut lua_State) -> c_int {
    native_log_inner(L, 1, false)
}

unsafe extern "C" fn native_log(L: *mut lua_State) -> c_int {
    native_log_inner(L, 1, true)
}

unsafe fn native_log_inner(L: *mut lua_State, default_level: i32, parse_level: bool) -> c_int {
    let Some(plugin_idx) = current_plugin_index(L) else {
        return 0;
    };
    let Some(host) = global_host() else {
        return 0;
    };
    let plugin_name = host
        .plugin_name_from_index(plugin_idx)
        .unwrap_or_else(|| "unknown-plugin".to_owned());

    let mut level = default_level;
    let mut first_arg = 1;
    if parse_level && lua_gettop(L) >= 1 {
        let arg0 = lua_type(L, 1);
        if arg0 == LUA_TNUMBER {
            let mut isnum = 0;
            let parsed = lua_tointegerx(L, 1, &mut isnum);
            if isnum != 0 {
                level = parsed as i32;
                first_arg = 2;
            }
        } else if arg0 == LUA_TSTRING {
            if let Some(text) = lua_value_string(L, 1) {
                match text.to_ascii_lowercase().as_str() {
                    "debug" => {
                        level = 0;
                        first_arg = 2;
                    }
                    "info" => {
                        level = 1;
                        first_arg = 2;
                    }
                    "warning" | "warn" => {
                        level = 2;
                        first_arg = 2;
                    }
                    "critical" | "error" => {
                        level = 3;
                        first_arg = 2;
                    }
                    _ => {}
                }
            }
        }
    }

    let mut parts = Vec::new();
    let top = lua_gettop(L);
    for i in first_arg..=top {
        parts.push(lua_value_debug_string(L, i));
    }
    let message = parts.join(" ");
    match level {
        0 => debug!("[plugin:{}] {}", plugin_name, message),
        1 => info!("[plugin:{}] {}", plugin_name, message),
        2 => warn!("[plugin:{}] {}", plugin_name, message),
        3 => error!("[plugin:{}] {}", plugin_name, message),
        _ => info!("[plugin:{}] {}", plugin_name, message),
    }
    0
}

unsafe extern "C" fn native_register_command(L: *mut lua_State) -> c_int {
    let Some(plugin_idx) = current_plugin_index(L) else {
        lua_pushboolean(L, 0);
        return 1;
    };
    let Some(host) = global_host() else {
        lua_pushboolean(L, 0);
        return 1;
    };

    if lua_gettop(L) < 2 || lua_type(L, 1) != LUA_TSTRING || lua_type(L, 2) != LUA_TFUNCTION {
        lua_pushboolean(L, 0);
        return 1;
    }

    let name = normalize_command_name(&lua_value_string(L, 1).unwrap_or_default());
    if name.is_empty() {
        lua_pushboolean(L, 0);
        return 1;
    }

    let mut meta = PluginCommandInfo {
        name: name.clone(),
        usage: format!("/{name}"),
        summary: "Plugin command".to_owned(),
        aliases: Vec::new(),
    };
    if lua_gettop(L) >= 3 && lua_type(L, 3) == LUA_TTABLE {
        meta = command_info_from_meta(L, 3, &name);
    }

    {
        let index_guard = host.command_index.read().unwrap_or_else(|p| p.into_inner());
        if index_guard.contains_key(&name.to_ascii_lowercase())
            || meta
                .aliases
                .iter()
                .map(|alias| alias.to_ascii_lowercase())
                .any(|alias| index_guard.contains_key(&alias))
        {
            lua_pushboolean(L, 0);
            return 1;
        }
    }

    let Some(runtime) = host.runtime_by_index(plugin_idx) else {
        lua_pushboolean(L, 0);
        return 1;
    };
    let mut guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
    if guard.commands.contains_key(&name) {
        lua_pushboolean(L, 0);
        return 1;
    }

    lua_pushvalue(L, 2);
    let handler_ref = luaL_ref(L, LUA_REGISTRYINDEX);
    guard.commands.insert(
        name.clone(),
        RegisteredCommand {
            info: meta.clone(),
            handler_ref,
        },
    );
    guard.command_count.fetch_add(1, Ordering::SeqCst);
    for alias in meta.aliases.iter().map(|s| normalize_command_name(s)) {
        guard.aliases.insert(alias, name.clone());
    }
    drop(guard);

    let mut index = host
        .command_index
        .write()
        .unwrap_or_else(|p| p.into_inner());
    index.insert(
        name.to_ascii_lowercase(),
        PluginCommandIndex {
            plugin_idx,
            canonical_name: name.clone(),
        },
    );
    if let Some(plugin_name) = host.plugin_name_from_index(plugin_idx) {
        info!("plugins: registered /{} from {}", name, plugin_name);
    }

    lua_pushboolean(L, 1);
    1
}

unsafe extern "C" fn native_register_callback(L: *mut lua_State) -> c_int {
    let Some(plugin_idx) = current_plugin_index(L) else {
        return 0;
    };
    let Some(host) = global_host() else {
        return 0;
    };
    if lua_gettop(L) < 2 || lua_type(L, 2) != LUA_TFUNCTION {
        return 0;
    }
    let Some(kind) = event_kind_from_value(L, 1) else {
        return 0;
    };
    let Some(runtime) = host.runtime_by_index(plugin_idx) else {
        return 0;
    };
    let mut guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
    lua_pushvalue(L, 2);
    let callback_ref = luaL_ref(L, LUA_REGISTRYINDEX);
    guard.callbacks.entry(kind).or_default().push(callback_ref);
    0
}

unsafe extern "C" fn native_later(L: *mut lua_State) -> c_int {
    let Some(plugin_idx) = current_plugin_index(L) else {
        return 0;
    };
    let Some(host) = global_host() else {
        return 0;
    };
    if lua_gettop(L) < 2 || lua_type(L, 1) != LUA_TFUNCTION {
        return 0;
    }
    let delay_ms = lua_value_int(L, 2).unwrap_or(0).max(0) as u64;
    let Some(runtime) = host.runtime_by_index(plugin_idx) else {
        return 0;
    };
    lua_pushvalue(L, 1);
    let callback_ref = luaL_ref(L, LUA_REGISTRYINDEX);
    let vm_key = {
        let guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
        guard.vm as usize
    };
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(delay_ms));
        host.send_command(AppCommand::RunPluginCallback {
            vm_key,
            callback_ref,
        });
    });
    0
}

impl LuaPluginHost {
    fn run_plugin_callback_inner(&self, vm_key: usize, callback_ref: c_int) {
        let Some(plugin_idx) = plugin_state_index()
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(&vm_key)
            .copied()
        else {
            return;
        };
        let Some(runtime) = self.runtime_by_index(plugin_idx) else {
            return;
        };
        let (vm, plugin_name) = {
            let guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
            (guard.vm, guard.manifest.name.clone())
        };
        let result = unsafe { call_lua_zero_arg(vm, callback_ref) };
        match result {
            Ok(()) => {}
            Err(err) => warn!("plugins: delayed callback in {} failed: {err}", plugin_name),
        }
        unsafe {
            luaL_unref(vm, LUA_REGISTRYINDEX, callback_ref);
        }
    }
}

unsafe extern "C" fn native_current_account(L: *mut lua_State) -> c_int {
    let Some(host) = global_host() else {
        lua_createtable(L, 0, 0);
        return 1;
    };
    push_account_table(L, &host.current_auth_snapshot());
    1
}

unsafe extern "C" fn native_channel_by_name(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        lua_createtable(L, 0, 0);
        return 1;
    }
    let Some(name) = lua_value_string(L, 1) else {
        lua_createtable(L, 0, 0);
        return 1;
    };
    push_channel_table(L, &ChannelId::new(name));
    1
}

unsafe extern "C" fn native_current_channel(L: *mut lua_State) -> c_int {
    let Some(host) = global_host() else {
        lua_pushnil(L);
        return 1;
    };
    if let Some(channel) = host.current_channel_snapshot() {
        push_channel_table(L, &channel);
    } else {
        lua_pushnil(L);
    }
    1
}

unsafe extern "C" fn native_send_message(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 2 {
        return 0;
    }
    let Some(host) = global_host() else {
        return 0;
    };
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let Some(text) = lua_value_string(L, 2) else {
        return 0;
    };
    let (reply_to_msg_id, reply) = if lua_gettop(L) >= 3 && lua_type(L, 3) == LUA_TTABLE {
        (
            lua_table_string_strict(L, 3, "reply_to_msg_id"),
            lua_table_reply_info(L, 3, "reply"),
        )
    } else {
        (None, None)
    };
    host.send_command(AppCommand::SendMessage {
        channel,
        text,
        reply_to_msg_id,
        reply,
    });
    0
}

unsafe extern "C" fn native_add_system_message(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 2 {
        return 0;
    }
    let Some(host) = global_host() else {
        return 0;
    };
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let Some(text) = lua_value_string(L, 2) else {
        return 0;
    };
    host.send_command(AppCommand::InjectLocalMessage { channel, text });
    0
}

unsafe extern "C" fn native_clear_messages(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(host) = global_host() else {
        return 0;
    };
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    host.send_command(AppCommand::ClearLocalMessages { channel });
    0
}

unsafe extern "C" fn native_open_url(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(host) = global_host() else {
        return 0;
    };
    let Some(url) = lua_value_string(L, 1) else {
        return 0;
    };
    host.send_command(AppCommand::OpenUrl { url });
    0
}

unsafe extern "C" fn native_plugin_dir(L: *mut lua_State) -> c_int {
    let Some(plugin_idx) = current_plugin_index(L) else {
        lua_pushnil(L);
        return 1;
    };
    let Some(host) = global_host() else {
        lua_pushnil(L);
        return 1;
    };
    if let Some(runtime) = host.runtime_by_index(plugin_idx) {
        let guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
        push_path_string(L, &guard.root_dir);
    } else {
        lua_pushnil(L);
    }
    1
}

unsafe extern "C" fn native_plugin_data_dir(L: *mut lua_State) -> c_int {
    let Some(plugin_idx) = current_plugin_index(L) else {
        lua_pushnil(L);
        return 1;
    };
    let Some(host) = global_host() else {
        lua_pushnil(L);
        return 1;
    };
    if let Some(runtime) = host.runtime_by_index(plugin_idx) {
        let guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
        push_path_string(L, &guard.data_dir);
    } else {
        lua_pushnil(L);
    }
    1
}

unsafe extern "C" fn native_use_24h_timestamps(L: *mut lua_State) -> c_int {
    let value = global_host()
        .map(|host| host.use_24h_timestamps())
        .unwrap_or(true);
    lua_pushboolean(L, if value { 1 } else { 0 });
    1
}

unsafe extern "C" fn native_session_started_ms(L: *mut lua_State) -> c_int {
    let value = global_host()
        .map(|host| host.session_started_unix_ms())
        .unwrap_or(0);
    lua_pushinteger(L, value);
    1
}

unsafe extern "C" fn native_ui_register_window(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(plugin_idx) = current_plugin_index(L) else {
        return 0;
    };
    let Some(host) = global_host() else {
        return 0;
    };
    if let Some(spec) = lua_table_ui_window_spec(L, 1, None) {
        host.set_plugin_window_spec(plugin_idx, spec);
    }
    0
}

unsafe extern "C" fn native_ui_update_window(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 2 {
        return 0;
    }
    let Some(plugin_idx) = current_plugin_index(L) else {
        return 0;
    };
    let Some(host) = global_host() else {
        return 0;
    };
    let Some(id) = lua_value_string(L, 1) else {
        return 0;
    };
    if let Some(spec) = lua_table_ui_window_spec(L, 2, Some(&id)) {
        host.set_plugin_window_spec(plugin_idx, spec);
    }
    0
}

unsafe extern "C" fn native_ui_open_window(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(plugin_idx) = current_plugin_index(L) else {
        return 0;
    };
    let Some(host) = global_host() else {
        return 0;
    };
    let Some(id) = lua_value_string(L, 1) else {
        return 0;
    };
    if let Some(runtime) = host.runtime_by_index(plugin_idx) {
        let mut guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(window) = guard.windows.get_mut(&id) {
            window.open = true;
        }
    }
    0
}

unsafe extern "C" fn native_ui_close_window(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(plugin_idx) = current_plugin_index(L) else {
        return 0;
    };
    let Some(host) = global_host() else {
        return 0;
    };
    let Some(id) = lua_value_string(L, 1) else {
        return 0;
    };
    if let Some(runtime) = host.runtime_by_index(plugin_idx) {
        let mut guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(window) = guard.windows.get_mut(&id) {
            window.open = false;
        }
    }
    0
}

unsafe extern "C" fn native_ui_unregister_window(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(plugin_idx) = current_plugin_index(L) else {
        return 0;
    };
    let Some(host) = global_host() else {
        return 0;
    };
    if let Some(id) = lua_value_string(L, 1) {
        host.remove_plugin_window(plugin_idx, &id);
    }
    0
}

unsafe extern "C" fn native_ui_register_settings_page(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(plugin_idx) = current_plugin_index(L) else {
        return 0;
    };
    let Some(host) = global_host() else {
        return 0;
    };
    if let Some(spec) = lua_table_ui_settings_page_spec(L, 1, None) {
        host.set_plugin_settings_page_spec(plugin_idx, spec);
    }
    0
}

unsafe extern "C" fn native_ui_update_settings_page(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 2 {
        return 0;
    }
    let Some(plugin_idx) = current_plugin_index(L) else {
        return 0;
    };
    let Some(host) = global_host() else {
        return 0;
    };
    let Some(id) = lua_value_string(L, 1) else {
        return 0;
    };
    if let Some(spec) = lua_table_ui_settings_page_spec(L, 2, Some(&id)) {
        host.set_plugin_settings_page_spec(plugin_idx, spec);
    }
    0
}

unsafe extern "C" fn native_ui_unregister_settings_page(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(plugin_idx) = current_plugin_index(L) else {
        return 0;
    };
    let Some(host) = global_host() else {
        return 0;
    };
    if let Some(id) = lua_value_string(L, 1) {
        host.remove_plugin_settings_page(plugin_idx, &id);
    }
    0
}

unsafe extern "C" fn native_ui_register_host_panel(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(plugin_idx) = current_plugin_index(L) else {
        return 0;
    };
    let Some(host) = global_host() else {
        return 0;
    };
    if let Some(spec) = lua_table_ui_host_panel_spec(L, 1, None) {
        host.set_plugin_host_panel_spec(plugin_idx, spec);
    }
    0
}

unsafe extern "C" fn native_ui_update_host_panel(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 2 {
        return 0;
    }
    let Some(plugin_idx) = current_plugin_index(L) else {
        return 0;
    };
    let Some(host) = global_host() else {
        return 0;
    };
    let Some(id) = lua_value_string(L, 1) else {
        return 0;
    };
    if let Some(spec) = lua_table_ui_host_panel_spec(L, 2, Some(&id)) {
        host.set_plugin_host_panel_spec(plugin_idx, spec);
    }
    0
}

unsafe extern "C" fn native_ui_unregister_host_panel(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(plugin_idx) = current_plugin_index(L) else {
        return 0;
    };
    let Some(host) = global_host() else {
        return 0;
    };
    if let Some(id) = lua_value_string(L, 1) {
        host.remove_plugin_host_panel(plugin_idx, &id);
    }
    0
}

unsafe fn send_lua_command(cmd: AppCommand) {
    if let Some(host) = global_host() {
        host.send_command(cmd);
    }
}

fn lua_table_channel(L: *mut lua_State, idx: c_int, field: &str) -> Option<ChannelId> {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        let out = channel_from_value(L, -1);
        lua_pop(L, 1);
        out
    }
}

fn lua_table_table(L: *mut lua_State, idx: c_int, field: &str) -> bool {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        let is_table = lua_type(L, -1) == LUA_TTABLE;
        lua_pop(L, 1);
        is_table
    }
}

unsafe extern "C" fn native_reload_plugins(L: *mut lua_State) -> c_int {
    let _ = L;
    send_lua_command(AppCommand::ReloadPlugins);
    0
}

unsafe extern "C" fn native_join_channel(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    send_lua_command(AppCommand::JoinChannel { channel });
    0
}

unsafe extern "C" fn native_join_irc_channel(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let key = if lua_gettop(L) >= 2 {
        lua_value_string(L, 2)
    } else {
        None
    };
    if channel.is_irc() {
        send_lua_command(AppCommand::JoinIrcChannel { channel, key });
    }
    0
}

unsafe extern "C" fn native_leave_channel(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    send_lua_command(AppCommand::LeaveChannel { channel });
    0
}

unsafe extern "C" fn native_send_whisper(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 2 {
        return 0;
    }
    let Some(target_login) = lua_value_string(L, 1) else {
        return 0;
    };
    let Some(text) = lua_value_string(L, 2) else {
        return 0;
    };
    send_lua_command(AppCommand::SendWhisper { target_login, text });
    0
}

unsafe extern "C" fn native_show_user_card(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 2 {
        return 0;
    }
    let Some(login) = lua_value_string(L, 1) else {
        return 0;
    };
    let Some(channel) = channel_from_value(L, 2) else {
        return 0;
    };
    send_lua_command(AppCommand::ShowUserCard { login, channel });
    0
}

unsafe extern "C" fn native_fetch_image(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(url) = lua_value_string(L, 1) else {
        return 0;
    };
    send_lua_command(AppCommand::FetchImage { url });
    0
}

unsafe extern "C" fn native_fetch_link_preview(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(url) = lua_value_string(L, 1) else {
        return 0;
    };
    send_lua_command(AppCommand::FetchLinkPreview { url });
    0
}

unsafe extern "C" fn native_load_channel_emotes(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(channel_twitch_id) = lua_value_string(L, 1) else {
        return 0;
    };
    send_lua_command(AppCommand::LoadChannelEmotes { channel_twitch_id });
    0
}

unsafe extern "C" fn native_fetch_stream_status(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(login) = lua_value_string(L, 1) else {
        return 0;
    };
    send_lua_command(AppCommand::FetchStreamStatus { login });
    0
}

unsafe extern "C" fn native_fetch_user_profile(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(login) = lua_value_string(L, 1) else {
        return 0;
    };
    send_lua_command(AppCommand::FetchUserProfile { login });
    0
}

unsafe extern "C" fn native_fetch_ivr_logs(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 2 {
        return 0;
    }
    let Some(channel) = lua_value_string(L, 1) else {
        return 0;
    };
    let Some(username) = lua_value_string(L, 2) else {
        return 0;
    };
    send_lua_command(AppCommand::FetchIvrLogs { channel, username });
    0
}

unsafe extern "C" fn native_load_older_local_history(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 3 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let before_ts_ms = lua_value_int(L, 2).unwrap_or(0);
    let limit = lua_value_int(L, 3).unwrap_or(100).max(0) as usize;
    send_lua_command(AppCommand::LoadOlderLocalHistory {
        channel,
        before_ts_ms,
        limit,
    });
    0
}

unsafe extern "C" fn native_login(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(token) = lua_value_string(L, 1) else {
        return 0;
    };
    send_lua_command(AppCommand::Login { token });
    0
}

unsafe extern "C" fn native_logout(L: *mut lua_State) -> c_int {
    let _ = L;
    send_lua_command(AppCommand::Logout);
    0
}

unsafe extern "C" fn native_add_account(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(token) = lua_value_string(L, 1) else {
        return 0;
    };
    send_lua_command(AppCommand::AddAccount { token });
    0
}

unsafe extern "C" fn native_switch_account(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(username) = lua_value_string(L, 1) else {
        return 0;
    };
    send_lua_command(AppCommand::SwitchAccount { username });
    0
}

unsafe extern "C" fn native_remove_account(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(username) = lua_value_string(L, 1) else {
        return 0;
    };
    send_lua_command(AppCommand::RemoveAccount { username });
    0
}

unsafe extern "C" fn native_set_default_account(L: *mut lua_State) -> c_int {
    let username = lua_value_string(L, 1).unwrap_or_default();
    send_lua_command(AppCommand::SetDefaultAccount { username });
    0
}

unsafe extern "C" fn native_refresh_auth(L: *mut lua_State) -> c_int {
    let _ = L;
    send_lua_command(AppCommand::RefreshAuth);
    0
}

unsafe extern "C" fn native_set_irc_nick(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(nick) = lua_value_string(L, 1) else {
        return 0;
    };
    send_lua_command(AppCommand::SetIrcNick { nick });
    0
}

unsafe extern "C" fn native_set_irc_auth(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 2 {
        return 0;
    }
    let Some(nickserv_user) = lua_value_string(L, 1) else {
        return 0;
    };
    let Some(nickserv_pass) = lua_value_string(L, 2) else {
        return 0;
    };
    send_lua_command(AppCommand::SetIrcAuth {
        nickserv_user,
        nickserv_pass,
    });
    0
}

unsafe extern "C" fn native_set_beta_features(L: *mut lua_State) -> c_int {
    let kick_enabled = lua_table_bool(L, 1, "kick_enabled", false);
    let irc_enabled = lua_table_bool(L, 1, "irc_enabled", false);
    send_lua_command(AppCommand::SetBetaFeatures {
        kick_enabled,
        irc_enabled,
    });
    0
}

unsafe extern "C" fn native_set_always_on_top(L: *mut lua_State) -> c_int {
    let enabled = lua_value_bool(L, 1).unwrap_or(false);
    send_lua_command(AppCommand::SetAlwaysOnTop { enabled });
    0
}

unsafe extern "C" fn native_set_theme(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(theme) = lua_value_string(L, 1) else {
        return 0;
    };
    send_lua_command(AppCommand::SetTheme { theme });
    0
}

unsafe extern "C" fn native_set_chat_ui_behavior(L: *mut lua_State) -> c_int {
    let prevent_overlong_twitch_messages =
        lua_table_bool(L, 1, "prevent_overlong_twitch_messages", false);
    let collapse_long_messages = lua_table_bool(L, 1, "collapse_long_messages", false);
    let collapse_long_message_lines = lua_table_usize(L, 1, "collapse_long_message_lines", 1);
    let animations_when_focused = lua_table_bool(L, 1, "animations_when_focused", false);
    send_lua_command(AppCommand::SetChatUiBehavior {
        prevent_overlong_twitch_messages,
        collapse_long_messages,
        collapse_long_message_lines,
        animations_when_focused,
    });
    0
}

unsafe extern "C" fn native_set_general_settings(L: *mut lua_State) -> c_int {
    let show_timestamps = lua_table_bool(L, 1, "show_timestamps", false);
    let show_timestamp_seconds = lua_table_bool(L, 1, "show_timestamp_seconds", false);
    let use_24h_timestamps = lua_table_bool(L, 1, "use_24h_timestamps", true);
    let local_log_indexing_enabled = lua_table_bool(L, 1, "local_log_indexing_enabled", false);
    let auto_join = lua_table_string_list(L, 1, "auto_join");
    let highlights = lua_table_string_list(L, 1, "highlights");
    let ignores = lua_table_string_list(L, 1, "ignores");
    send_lua_command(AppCommand::SetGeneralSettings {
        show_timestamps,
        show_timestamp_seconds,
        use_24h_timestamps,
        local_log_indexing_enabled,
        auto_join,
        highlights,
        ignores,
    });
    0
}

unsafe extern "C" fn native_set_slash_usage_counts(L: *mut lua_State) -> c_int {
    if !lua_table_table(L, 1, "usage_counts") {
        return 0;
    }
    unsafe {
        let idx = lua_absindex(L, 1);
        lua_getfield(L, idx, cstring("usage_counts").as_ptr());
        let mut usage_counts = Vec::new();
        if lua_type(L, -1) == LUA_TTABLE {
            let len = lua_rawlen(L, -1);
            for i in 1..=len {
                lua_geti(L, -1, i as lua_Integer);
                if lua_type(L, -1) == LUA_TTABLE {
                    let name = lua_table_string(L, -1, "name")
                        .or_else(|| lua_table_string(L, -1, "command"))
                        .unwrap_or_default();
                    let count = lua_table_int(L, -1, "count", 0).max(0) as u32;
                    let normalized = name.trim().trim_start_matches('/').to_ascii_lowercase();
                    if !normalized.is_empty() {
                        usage_counts.push((normalized, count));
                    }
                }
                lua_pop(L, 1);
            }
        }
        lua_pop(L, 1);
        send_lua_command(AppCommand::SetSlashUsageCounts { usage_counts });
    }
    0
}

unsafe extern "C" fn native_set_emote_picker_preferences(L: *mut lua_State) -> c_int {
    let favorites = lua_table_string_list(L, 1, "favorites");
    let recent = lua_table_string_list(L, 1, "recent");
    let provider_boost = lua_table_string(L, 1, "provider_boost");
    send_lua_command(AppCommand::SetEmotePickerPreferences {
        favorites,
        recent,
        provider_boost,
    });
    0
}

unsafe extern "C" fn native_set_appearance_settings(L: *mut lua_State) -> c_int {
    let channel_layout = lua_table_string_value(L, 1, "channel_layout", "sidebar");
    let sidebar_visible = lua_table_bool(L, 1, "sidebar_visible", true);
    let analytics_visible = lua_table_bool(L, 1, "analytics_visible", false);
    let irc_status_visible = lua_table_bool(L, 1, "irc_status_visible", false);
    let tab_style = lua_table_string_value(L, 1, "tab_style", "compact");
    let show_tab_close_buttons = lua_table_bool(L, 1, "show_tab_close_buttons", true);
    let show_tab_live_indicators = lua_table_bool(L, 1, "show_tab_live_indicators", true);
    let split_header_show_title = lua_table_bool(L, 1, "split_header_show_title", true);
    let split_header_show_game = lua_table_bool(L, 1, "split_header_show_game", true);
    let split_header_show_viewer_count =
        lua_table_bool(L, 1, "split_header_show_viewer_count", false);
    send_lua_command(AppCommand::SetAppearanceSettings {
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
    });
    0
}

unsafe extern "C" fn native_set_highlight_rules(L: *mut lua_State) -> c_int {
    let idx = lua_absindex(L, 1);
    let mut rules = Vec::new();
    if lua_type(L, idx) == LUA_TTABLE {
        let len = lua_rawlen(L, idx);
        for i in 1..=len {
            lua_geti(L, idx, i as lua_Integer);
            if lua_type(L, -1) == LUA_TTABLE {
                let pattern = lua_table_string(L, -1, "pattern").unwrap_or_default();
                let color = if lua_table_table(L, -1, "color") {
                    lua_getfield(L, -1, cstring("color").as_ptr());
                    let mut rgb = [0u8; 3];
                    for i in 0..3 {
                        lua_geti(L, -1, (i + 1) as lua_Integer);
                        rgb[i] = lua_value_int(L, -1).unwrap_or(0).clamp(0, 255) as u8;
                        lua_pop(L, 1);
                    }
                    lua_pop(L, 1);
                    Some(rgb)
                } else {
                    None
                };
                rules.push(HighlightRule {
                    pattern,
                    is_regex: lua_table_bool(L, -1, "is_regex", false),
                    case_sensitive: lua_table_bool(L, -1, "case_sensitive", false),
                    enabled: lua_table_bool(L, -1, "enabled", true),
                    show_in_mentions: lua_table_bool(L, -1, "show_in_mentions", false),
                    color,
                    has_alert: lua_table_bool(L, -1, "has_alert", false),
                    has_sound: lua_table_bool(L, -1, "has_sound", false),
                    sound_url: lua_table_string(L, -1, "sound_url"),
                });
            }
            lua_pop(L, 1);
        }
    }
    send_lua_command(AppCommand::SetHighlightRules { rules });
    0
}

unsafe extern "C" fn native_set_filter_records(L: *mut lua_State) -> c_int {
    let idx = lua_absindex(L, 1);
    let mut records = Vec::new();
    if lua_type(L, idx) == LUA_TTABLE {
        let len = lua_rawlen(L, idx);
        for i in 1..=len {
            lua_geti(L, idx, i as lua_Integer);
            if lua_type(L, -1) == LUA_TTABLE {
                let scope = if let Some(channel) = lua_table_channel(L, -1, "channel") {
                    crust_core::model::filters::FilterScope::Channel(channel)
                } else {
                    crust_core::model::filters::FilterScope::Global
                };
                let action = match lua_table_string(L, -1, "action")
                    .unwrap_or_else(|| "Hide".to_owned())
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "dim" => crust_core::model::filters::FilterAction::Dim,
                    _ => crust_core::model::filters::FilterAction::Hide,
                };
                records.push(FilterRecord {
                    name: lua_table_string_value(L, -1, "name", ""),
                    pattern: lua_table_string_value(L, -1, "pattern", ""),
                    is_regex: lua_table_bool(L, -1, "is_regex", false),
                    case_sensitive: lua_table_bool(L, -1, "case_sensitive", false),
                    enabled: lua_table_bool(L, -1, "enabled", true),
                    scope,
                    action,
                    filter_sender: lua_table_bool(L, -1, "filter_sender", false),
                });
            }
            lua_pop(L, 1);
        }
    }
    send_lua_command(AppCommand::SetFilterRecords { records });
    0
}

unsafe extern "C" fn native_set_mod_action_presets(L: *mut lua_State) -> c_int {
    let idx = lua_absindex(L, 1);
    let mut presets = Vec::new();
    if lua_type(L, idx) == LUA_TTABLE {
        let len = lua_rawlen(L, idx);
        for i in 1..=len {
            lua_geti(L, idx, i as lua_Integer);
            if lua_type(L, -1) == LUA_TTABLE {
                presets.push(ModActionPreset {
                    label: lua_table_string_value(L, -1, "label", ""),
                    command_template: lua_table_string_value(L, -1, "command_template", ""),
                    icon_url: lua_table_string(L, -1, "icon_url"),
                });
            }
            lua_pop(L, 1);
        }
    }
    send_lua_command(AppCommand::SetModActionPresets { presets });
    0
}

unsafe extern "C" fn native_set_notification_settings(L: *mut lua_State) -> c_int {
    let desktop_notifications_enabled =
        lua_table_bool(L, 1, "desktop_notifications_enabled", false);
    send_lua_command(AppCommand::SetNotificationSettings {
        desktop_notifications_enabled,
    });
    0
}

unsafe extern "C" fn native_timeout_user(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 4 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let Some(login) = lua_value_string(L, 2) else {
        return 0;
    };
    let Some(user_id) = lua_value_string(L, 3) else {
        return 0;
    };
    let seconds = lua_value_int(L, 4).unwrap_or(0).max(0) as u32;
    let reason = lua_value_string(L, 5);
    send_lua_command(AppCommand::TimeoutUser {
        channel,
        login,
        user_id,
        seconds,
        reason,
    });
    0
}

unsafe extern "C" fn native_ban_user(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 3 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let Some(login) = lua_value_string(L, 2) else {
        return 0;
    };
    let Some(user_id) = lua_value_string(L, 3) else {
        return 0;
    };
    let reason = lua_value_string(L, 4);
    send_lua_command(AppCommand::BanUser {
        channel,
        login,
        user_id,
        reason,
    });
    0
}

unsafe extern "C" fn native_unban_user(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 3 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let Some(login) = lua_value_string(L, 2) else {
        return 0;
    };
    let Some(user_id) = lua_value_string(L, 3) else {
        return 0;
    };
    send_lua_command(AppCommand::UnbanUser {
        channel,
        login,
        user_id,
    });
    0
}

unsafe extern "C" fn native_warn_user(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 4 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let Some(login) = lua_value_string(L, 2) else {
        return 0;
    };
    let Some(user_id) = lua_value_string(L, 3) else {
        return 0;
    };
    let Some(reason) = lua_value_string(L, 4) else {
        return 0;
    };
    send_lua_command(AppCommand::WarnUser {
        channel,
        login,
        user_id,
        reason,
    });
    0
}

unsafe extern "C" fn native_set_suspicious_user(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 4 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let Some(login) = lua_value_string(L, 2) else {
        return 0;
    };
    let Some(user_id) = lua_value_string(L, 3) else {
        return 0;
    };
    let restricted = lua_value_bool(L, 4).unwrap_or(false);
    send_lua_command(AppCommand::SetSuspiciousUser {
        channel,
        login,
        user_id,
        restricted,
    });
    0
}

unsafe extern "C" fn native_clear_suspicious_user(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 3 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let Some(login) = lua_value_string(L, 2) else {
        return 0;
    };
    let Some(user_id) = lua_value_string(L, 3) else {
        return 0;
    };
    send_lua_command(AppCommand::ClearSuspiciousUser {
        channel,
        login,
        user_id,
    });
    0
}

unsafe extern "C" fn native_resolve_automod_message(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 4 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let Some(message_id) = lua_value_string(L, 2) else {
        return 0;
    };
    let Some(sender_user_id) = lua_value_string(L, 3) else {
        return 0;
    };
    let action = lua_value_string(L, 4).unwrap_or_else(|| "ALLOW".to_owned());
    send_lua_command(AppCommand::ResolveAutoModMessage {
        channel,
        message_id,
        sender_user_id,
        action,
    });
    0
}

unsafe extern "C" fn native_fetch_unban_requests(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    send_lua_command(AppCommand::FetchUnbanRequests { channel });
    0
}

unsafe extern "C" fn native_resolve_unban_request(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 3 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let Some(request_id) = lua_value_string(L, 2) else {
        return 0;
    };
    let approve = lua_value_bool(L, 3).unwrap_or(false);
    let resolution_text = lua_value_string(L, 4);
    send_lua_command(AppCommand::ResolveUnbanRequest {
        channel,
        request_id,
        approve,
        resolution_text,
    });
    0
}

unsafe extern "C" fn native_open_moderation_tools(L: *mut lua_State) -> c_int {
    let channel = if lua_gettop(L) >= 1 {
        channel_from_value(L, 1)
    } else {
        None
    };
    send_lua_command(AppCommand::OpenModerationTools { channel });
    0
}

unsafe extern "C" fn native_update_reward_redemption_status(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 6 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let Some(reward_id) = lua_value_string(L, 2) else {
        return 0;
    };
    let Some(redemption_id) = lua_value_string(L, 3) else {
        return 0;
    };
    let Some(status) = lua_value_string(L, 4) else {
        return 0;
    };
    let Some(user_login) = lua_value_string(L, 5) else {
        return 0;
    };
    let Some(reward_title) = lua_value_string(L, 6) else {
        return 0;
    };
    send_lua_command(AppCommand::UpdateRewardRedemptionStatus {
        channel,
        reward_id,
        redemption_id,
        status,
        user_login,
        reward_title,
    });
    0
}

unsafe extern "C" fn native_delete_message(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 2 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let Some(message_id) = lua_value_string(L, 2) else {
        return 0;
    };
    send_lua_command(AppCommand::DeleteMessage {
        channel,
        message_id,
    });
    0
}

unsafe extern "C" fn native_clear_user_messages_locally(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 2 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let Some(login) = lua_value_string(L, 2) else {
        return 0;
    };
    send_lua_command(AppCommand::ClearUserMessagesLocally { channel, login });
    0
}

unsafe extern "C" fn native_create_poll(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 4 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let Some(title) = lua_value_string(L, 2) else {
        return 0;
    };
    let choices = lua_array_string_list(L, 3);
    let duration_secs = lua_value_int(L, 4).unwrap_or(0).max(0) as u32;
    let channel_points_per_vote = lua_value_int(L, 5).map(|v| v.max(0) as u32);
    send_lua_command(AppCommand::CreatePoll {
        channel,
        title,
        choices,
        duration_secs,
        channel_points_per_vote,
    });
    0
}

unsafe extern "C" fn native_end_poll(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 2 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let Some(status) = lua_value_string(L, 2) else {
        return 0;
    };
    send_lua_command(AppCommand::EndPoll { channel, status });
    0
}

unsafe extern "C" fn native_create_prediction(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 4 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let Some(title) = lua_value_string(L, 2) else {
        return 0;
    };
    let outcomes = lua_array_string_list(L, 3);
    let duration_secs = lua_value_int(L, 4).unwrap_or(0).max(0) as u32;
    send_lua_command(AppCommand::CreatePrediction {
        channel,
        title,
        outcomes,
        duration_secs,
    });
    0
}

unsafe extern "C" fn native_lock_prediction(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    send_lua_command(AppCommand::LockPrediction { channel });
    0
}

unsafe extern "C" fn native_resolve_prediction(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 2 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let winning_outcome_index = lua_value_int(L, 2).unwrap_or(1).max(1) as usize;
    send_lua_command(AppCommand::ResolvePrediction {
        channel,
        winning_outcome_index,
    });
    0
}

unsafe extern "C" fn native_cancel_prediction(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    send_lua_command(AppCommand::CancelPrediction { channel });
    0
}

unsafe extern "C" fn native_start_commercial(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 2 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let length_secs = lua_value_int(L, 2).unwrap_or(0).max(0) as u32;
    send_lua_command(AppCommand::StartCommercial {
        channel,
        length_secs,
    });
    0
}

unsafe extern "C" fn native_create_stream_marker(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 1 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let description = lua_value_string(L, 2);
    send_lua_command(AppCommand::CreateStreamMarker {
        channel,
        description,
    });
    0
}

unsafe extern "C" fn native_send_announcement(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 2 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let Some(message) = lua_value_string(L, 2) else {
        return 0;
    };
    let color = lua_value_string(L, 3);
    send_lua_command(AppCommand::SendAnnouncement {
        channel,
        message,
        color,
    });
    0
}

unsafe extern "C" fn native_send_shoutout(L: *mut lua_State) -> c_int {
    if lua_gettop(L) < 2 {
        return 0;
    }
    let Some(channel) = channel_from_value(L, 1) else {
        return 0;
    };
    let Some(target_login) = lua_value_string(L, 2) else {
        return 0;
    };
    send_lua_command(AppCommand::SendShoutout {
        channel,
        target_login,
    });
    0
}

fn make_command_context(
    L: *mut lua_State,
    host: &LuaPluginHost,
    invocation: &PluginCommandInvocation,
) -> c_int {
    unsafe {
        lua_createtable(L, 0, 0);
        set_field_string(L, -1, "command", &invocation.command);
        set_field_string(L, -1, "raw_text", &invocation.raw_text);
        set_field_string(L, -1, "channel_name", invocation.channel.display_name());
        push_channel_table(L, &invocation.channel);
        lua_setfield(L, -2, cstring("channel").as_ptr());
        push_account_table(L, &host.current_auth_snapshot());
        lua_setfield(L, -2, cstring("account").as_ptr());

        lua_createtable(L, invocation.words.len() as c_int, 0);
        for (idx, word) in invocation.words.iter().enumerate() {
            set_list_string(L, -1, (idx + 1) as lua_Integer, word);
        }
        lua_setfield(L, -2, cstring("words").as_ptr());

        if let Some(reply_to_msg_id) = &invocation.reply_to_msg_id {
            set_field_string(L, -1, "reply_to_msg_id", reply_to_msg_id);
        }
        if let Some(reply) = &invocation.reply {
            lua_createtable(L, 0, 0);
            set_field_string(L, -1, "parent_msg_id", &reply.parent_msg_id);
            set_field_string(L, -1, "parent_user_login", &reply.parent_user_login);
            set_field_string(L, -1, "parent_display_name", &reply.parent_display_name);
            set_field_string(L, -1, "parent_msg_body", &reply.parent_msg_body);
            lua_setfield(L, -2, cstring("reply").as_ptr());
        }
        lua_gettop(L)
    }
}

fn make_completion_event(
    L: *mut lua_State,
    request: &PluginCompletionRequest,
    host: &LuaPluginHost,
) -> c_int {
    unsafe {
        lua_createtable(L, 0, 0);
        set_field_string(L, -1, "query", &request.query);
        set_field_string(L, -1, "full_text_content", &request.full_text_content);
        set_field_int(L, -1, "cursor_position", request.cursor_position as i64);
        set_field_bool(L, -1, "is_first_word", request.is_first_word);
        if let Some(channel) = &request.channel {
            push_channel_table(L, channel);
            lua_setfield(L, -2, cstring("channel").as_ptr());
        }
        let _ = host;
        lua_gettop(L)
    }
}

unsafe fn call_lua_zero_arg(L: *mut lua_State, func_ref: c_int) -> Result<(), String> {
    lua_rawgeti(L, LUA_REGISTRYINDEX, func_ref as lua_Integer);
    let rc = lua_pcallk(L, 0, 0, 0, 0, None);
    if rc != LUA_OK {
        return Err(lua_error_text(L, rc));
    }
    Ok(())
}

unsafe fn call_completion_callback(
    L: *mut lua_State,
    func_ref: c_int,
    arg_index: c_int,
) -> Result<Option<PluginCompletionList>, String> {
    lua_rawgeti(L, LUA_REGISTRYINDEX, func_ref as lua_Integer);
    lua_pushvalue(L, arg_index);
    let rc = lua_pcallk(L, 1, 1, 0, 0, None);
    if rc != LUA_OK {
        return Err(lua_error_text(L, rc));
    }

    if lua_type(L, -1) != LUA_TTABLE {
        lua_pop(L, 1);
        return Ok(None);
    }
    let out = completion_list_from_table(L, -1);
    lua_pop(L, 1);
    Ok(out)
}

unsafe fn call_command_handler(
    L: *mut lua_State,
    func_ref: c_int,
    arg_index: c_int,
) -> Result<Option<String>, String> {
    lua_rawgeti(L, LUA_REGISTRYINDEX, func_ref as lua_Integer);
    lua_pushvalue(L, arg_index);
    let rc = lua_pcallk(L, 1, 1, 0, 0, None);
    if rc != LUA_OK {
        return Err(lua_error_text(L, rc));
    }
    let out = if lua_type(L, -1) == LUA_TSTRING {
        lua_value_string(L, -1)
    } else {
        None
    };
    lua_pop(L, 1);
    Ok(out)
}

fn completion_list_from_table(L: *mut lua_State, idx: c_int) -> Option<PluginCompletionList> {
    unsafe {
        let idx = lua_absindex(L, idx);
        let mut values = Vec::new();
        lua_getfield(L, idx, cstring("values").as_ptr());
        if lua_type(L, -1) == LUA_TTABLE {
            let len = lua_rawlen(L, -1);
            for i in 1..=len {
                lua_geti(L, -1, i as lua_Integer);
                if let Some(s) = lua_value_string(L, -1) {
                    values.push(s);
                }
                lua_pop(L, 1);
            }
        }
        lua_pop(L, 1);

        lua_getfield(L, idx, cstring("hide_others").as_ptr());
        let hide_others = lua_value_bool(L, -1).unwrap_or(false);
        lua_pop(L, 1);

        Some(PluginCompletionList {
            values,
            hide_others,
        })
    }
}

fn command_info_from_meta(L: *mut lua_State, idx: c_int, name: &str) -> PluginCommandInfo {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring("usage").as_ptr());
        let usage = lua_value_string(L, -1).unwrap_or_else(|| format!("/{name}"));
        lua_pop(L, 1);

        lua_getfield(L, idx, cstring("summary").as_ptr());
        let summary = lua_value_string(L, -1).unwrap_or_else(|| "Plugin command".to_owned());
        lua_pop(L, 1);

        let mut out = Vec::new();
        lua_getfield(L, idx, cstring("aliases").as_ptr());
        if lua_type(L, -1) == LUA_TTABLE {
            let len = lua_rawlen(L, -1);
            for i in 1..=len {
                lua_geti(L, -1, i as lua_Integer);
                if let Some(s) = lua_value_string(L, -1) {
                    out.push(s);
                }
                lua_pop(L, 1);
            }
        }
        lua_pop(L, 1);
        PluginCommandInfo {
            name: name.to_owned(),
            usage,
            summary,
            aliases: out,
        }
    }
}

fn push_account_table(L: *mut lua_State, auth: &PluginAuthSnapshot) {
    unsafe {
        lua_createtable(L, 0, 0);
        set_field_bool(L, -1, "logged_in", auth.logged_in);
        if let Some(username) = auth.username.as_ref() {
            set_field_string(L, -1, "username", username);
        }
        if let Some(user_id) = auth.user_id.as_ref() {
            set_field_string(L, -1, "user_id", user_id);
        }
        if let Some(display_name) = auth.display_name.as_ref() {
            set_field_string(L, -1, "display_name", display_name);
        }
    }
}

fn push_channel_table(L: *mut lua_State, channel: &ChannelId) {
    unsafe {
        lua_createtable(L, 0, 0);
        set_field_string(L, -1, "name", channel.display_name());
        set_field_string(L, -1, "display_name", channel.display_name());
        set_field_string(
            L,
            -1,
            "platform",
            match channel.platform() {
                Platform::Twitch => "twitch",
                Platform::Kick => "kick",
                Platform::Irc => "irc",
            },
        );
        set_field_string(L, -1, "id", channel.as_str());
        set_field_bool(L, -1, "is_twitch", channel.is_twitch());
        set_field_bool(L, -1, "is_irc", channel.is_irc());
        set_field_bool(L, -1, "is_kick", channel.is_kick());
        if let Some(host) = global_host() {
            let snap = host.plugin_snapshot_for(channel);
            set_field_bool(L, -1, "is_joined", snap.is_joined);
            set_field_bool(L, -1, "is_mod", snap.is_mod);
            set_field_bool(L, -1, "is_vip", snap.is_vip);
            set_field_bool(L, -1, "is_broadcaster", snap.is_broadcaster);
        }
    }
}

fn push_string_array(L: *mut lua_State, values: &[String]) {
    unsafe {
        lua_createtable(L, values.len() as c_int, 0);
        for (idx, value) in values.iter().enumerate() {
            set_list_string(L, -1, (idx + 1) as lua_Integer, value);
        }
    }
}

fn push_badge_table(L: *mut lua_State, badge: &Badge) {
    unsafe {
        lua_createtable(L, 0, 0);
        set_field_string(L, -1, "name", &badge.name);
        set_field_string(L, -1, "version", &badge.version);
        if let Some(url) = badge.url.as_ref() {
            set_field_string(L, -1, "url", url);
        }
    }
}

fn push_sender_name_paint_shadow_table(L: *mut lua_State, shadow: &SenderNamePaintShadow) {
    unsafe {
        lua_createtable(L, 0, 0);
        set_field_int(L, -1, "x_offset", shadow.x_offset as i64);
        set_field_int(L, -1, "y_offset", shadow.y_offset as i64);
        set_field_int(L, -1, "radius", shadow.radius as i64);
        set_field_string(L, -1, "color", &shadow.color);
    }
}

fn push_sender_name_paint_stop_table(L: *mut lua_State, stop: &SenderNamePaintStop) {
    unsafe {
        lua_createtable(L, 0, 0);
        set_field_int(L, -1, "at", (stop.at * 1000.0) as i64);
        set_field_string(L, -1, "color", &stop.color);
    }
}

fn push_sender_name_paint_table(L: *mut lua_State, paint: &SenderNamePaint) {
    unsafe {
        lua_createtable(L, 0, 0);
        set_field_string(L, -1, "function", &paint.function);
        if let Some(angle) = paint.angle {
            set_field_int(L, -1, "angle", angle as i64);
        }
        set_field_bool(L, -1, "repeat", paint.repeat);
        if let Some(image_url) = paint.image_url.as_ref() {
            set_field_string(L, -1, "image_url", image_url);
        }
        lua_createtable(L, paint.shadows.len() as c_int, 0);
        for (idx, shadow) in paint.shadows.iter().enumerate() {
            push_sender_name_paint_shadow_table(L, shadow);
            lua_seti(L, -2, (idx + 1) as lua_Integer);
        }
        lua_setfield(L, -2, cstring("shadows").as_ptr());
        lua_createtable(L, paint.stops.len() as c_int, 0);
        for (idx, stop) in paint.stops.iter().enumerate() {
            push_sender_name_paint_stop_table(L, stop);
            lua_seti(L, -2, (idx + 1) as lua_Integer);
        }
        lua_setfield(L, -2, cstring("stops").as_ptr());
    }
}

fn push_sender_table(L: *mut lua_State, sender: &Sender) {
    unsafe {
        lua_createtable(L, 0, 0);
        set_field_string(L, -1, "user_id", &sender.user_id.0);
        set_field_string(L, -1, "login", &sender.login);
        set_field_string(L, -1, "display_name", &sender.display_name);
        if let Some(color) = sender.color.as_ref() {
            set_field_string(L, -1, "color", color);
        }
        if let Some(paint) = sender.name_paint.as_ref() {
            push_sender_name_paint_table(L, paint);
            lua_setfield(L, -2, cstring("name_paint").as_ptr());
        }
        lua_createtable(L, sender.badges.len() as c_int, 0);
        for (idx, badge) in sender.badges.iter().enumerate() {
            push_badge_table(L, badge);
            lua_seti(L, -2, (idx + 1) as lua_Integer);
        }
        lua_setfield(L, -2, cstring("badges").as_ptr());
    }
}

fn push_message_flags_table(L: *mut lua_State, flags: &MessageFlags) {
    unsafe {
        lua_createtable(L, 0, 0);
        set_field_bool(L, -1, "is_action", flags.is_action);
        set_field_bool(L, -1, "is_highlighted", flags.is_highlighted);
        set_field_bool(L, -1, "is_deleted", flags.is_deleted);
        set_field_bool(L, -1, "is_first_msg", flags.is_first_msg);
        set_field_bool(L, -1, "is_pinned", flags.is_pinned);
        set_field_bool(L, -1, "is_self", flags.is_self);
        set_field_bool(L, -1, "is_mention", flags.is_mention);
        if let Some(custom_reward_id) = flags.custom_reward_id.as_ref() {
            set_field_string(L, -1, "custom_reward_id", custom_reward_id);
        }
        set_field_bool(L, -1, "is_history", flags.is_history);
    }
}

fn push_reply_info_table(L: *mut lua_State, reply: &ReplyInfo) {
    unsafe {
        lua_createtable(L, 0, 0);
        set_field_string(L, -1, "parent_msg_id", &reply.parent_msg_id);
        set_field_string(L, -1, "parent_user_login", &reply.parent_user_login);
        set_field_string(L, -1, "parent_display_name", &reply.parent_display_name);
        set_field_string(L, -1, "parent_msg_body", &reply.parent_msg_body);
    }
}

fn push_twitch_emote_table(L: *mut lua_State, emote: &TwitchEmotePos) {
    unsafe {
        lua_createtable(L, 0, 0);
        set_field_string(L, -1, "id", &emote.id);
        set_field_int(L, -1, "start", emote.start as i64);
        set_field_int(L, -1, "end", emote.end as i64);
    }
}

fn push_emote_catalog_entry_table(L: *mut lua_State, emote: &EmoteCatalogEntry) {
    unsafe {
        lua_createtable(L, 0, 0);
        set_field_string(L, -1, "code", &emote.code);
        set_field_string(L, -1, "provider", &emote.provider);
        set_field_string(L, -1, "url", &emote.url);
        set_field_string(L, -1, "scope", &emote.scope);
    }
}

fn push_span_table(L: *mut lua_State, span: &Span) {
    unsafe {
        lua_createtable(L, 0, 0);
        match span {
            Span::Text { text, is_action } => {
                set_field_string(L, -1, "type", "Text");
                set_field_string(L, -1, "text", text);
                set_field_bool(L, -1, "is_action", *is_action);
            }
            Span::Emote {
                id,
                code,
                url,
                url_hd,
                provider,
            } => {
                set_field_string(L, -1, "type", "Emote");
                set_field_string(L, -1, "id", id);
                set_field_string(L, -1, "code", code);
                set_field_string(L, -1, "url", url);
                if let Some(url_hd) = url_hd.as_ref() {
                    set_field_string(L, -1, "url_hd", url_hd);
                }
                set_field_string(L, -1, "provider", provider);
            }
            Span::Emoji { text, url } => {
                set_field_string(L, -1, "type", "Emoji");
                set_field_string(L, -1, "text", text);
                set_field_string(L, -1, "url", url);
            }
            Span::Badge { name, version } => {
                set_field_string(L, -1, "type", "Badge");
                set_field_string(L, -1, "name", name);
                set_field_string(L, -1, "version", version);
            }
            Span::Mention { login } => {
                set_field_string(L, -1, "type", "Mention");
                set_field_string(L, -1, "login", login);
            }
            Span::Url { text, url } => {
                set_field_string(L, -1, "type", "Url");
                set_field_string(L, -1, "text", text);
                set_field_string(L, -1, "url", url);
            }
        }
    }
}

fn push_msg_kind_table(L: *mut lua_State, kind: &MsgKind) {
    unsafe {
        lua_createtable(L, 0, 0);
        match kind {
            MsgKind::Chat => {
                set_field_string(L, -1, "type", "Chat");
            }
            MsgKind::Sub {
                display_name,
                months,
                plan,
                is_gift,
                sub_msg,
            } => {
                set_field_string(L, -1, "type", "Sub");
                set_field_string(L, -1, "display_name", display_name);
                set_field_int(L, -1, "months", *months as i64);
                set_field_string(L, -1, "plan", plan);
                set_field_bool(L, -1, "is_gift", *is_gift);
                set_field_string(L, -1, "sub_msg", sub_msg);
            }
            MsgKind::Raid {
                display_name,
                viewer_count,
            } => {
                set_field_string(L, -1, "type", "Raid");
                set_field_string(L, -1, "display_name", display_name);
                set_field_int(L, -1, "viewer_count", *viewer_count as i64);
            }
            MsgKind::Timeout { login, seconds } => {
                set_field_string(L, -1, "type", "Timeout");
                set_field_string(L, -1, "login", login);
                set_field_int(L, -1, "seconds", *seconds as i64);
            }
            MsgKind::Ban { login } => {
                set_field_string(L, -1, "type", "Ban");
                set_field_string(L, -1, "login", login);
            }
            MsgKind::ChatCleared => {
                set_field_string(L, -1, "type", "ChatCleared");
            }
            MsgKind::SystemInfo => {
                set_field_string(L, -1, "type", "SystemInfo");
            }
            MsgKind::ChannelPointsReward {
                user_login,
                reward_title,
                cost,
                reward_id,
                redemption_id,
                user_input,
                status,
            } => {
                set_field_string(L, -1, "type", "ChannelPointsReward");
                set_field_string(L, -1, "user_login", user_login);
                set_field_string(L, -1, "reward_title", reward_title);
                set_field_int(L, -1, "cost", *cost as i64);
                if let Some(reward_id) = reward_id.as_ref() {
                    set_field_string(L, -1, "reward_id", reward_id);
                }
                if let Some(redemption_id) = redemption_id.as_ref() {
                    set_field_string(L, -1, "redemption_id", redemption_id);
                }
                if let Some(user_input) = user_input.as_ref() {
                    set_field_string(L, -1, "user_input", user_input);
                }
                if let Some(status) = status.as_ref() {
                    set_field_string(L, -1, "status", status);
                }
            }
            MsgKind::SuspiciousUserMessage => {
                set_field_string(L, -1, "type", "SuspiciousUserMessage");
            }
            MsgKind::Bits { amount } => {
                set_field_string(L, -1, "type", "Bits");
                set_field_int(L, -1, "amount", *amount as i64);
            }
        }
    }
}

fn push_chat_message_table(L: *mut lua_State, message: &ChatMessage) {
    unsafe {
        lua_createtable(L, 0, 0);
        set_field_int(L, -1, "id", message.id.0 as i64);
        if let Some(server_id) = message.server_id.as_ref() {
            set_field_string(L, -1, "server_id", server_id);
        }
        set_field_string(L, -1, "timestamp", &message.timestamp.to_rfc3339());
        push_channel_table(L, &message.channel);
        lua_setfield(L, -2, cstring("channel").as_ptr());
        push_sender_table(L, &message.sender);
        lua_setfield(L, -2, cstring("sender").as_ptr());
        set_field_string(L, -1, "raw_text", &message.raw_text);
        lua_createtable(L, message.spans.len() as c_int, 0);
        for (idx, span) in message.spans.iter().enumerate() {
            push_span_table(L, span);
            lua_seti(L, -2, (idx + 1) as lua_Integer);
        }
        lua_setfield(L, -2, cstring("spans").as_ptr());
        lua_createtable(L, message.twitch_emotes.len() as c_int, 0);
        for (idx, twitch_emote) in message.twitch_emotes.iter().enumerate() {
            push_twitch_emote_table(L, twitch_emote);
            lua_seti(L, -2, (idx + 1) as lua_Integer);
        }
        lua_setfield(L, -2, cstring("twitch_emotes").as_ptr());
        push_message_flags_table(L, &message.flags);
        lua_setfield(L, -2, cstring("flags").as_ptr());
        if let Some(reply) = message.reply.as_ref() {
            push_reply_info_table(L, reply);
            lua_setfield(L, -2, cstring("reply").as_ptr());
        }
        push_msg_kind_table(L, &message.msg_kind);
        lua_setfield(L, -2, cstring("msg_kind").as_ptr());
    }
}

fn push_user_profile_table(L: *mut lua_State, profile: &UserProfile) {
    unsafe {
        lua_createtable(L, 0, 0);
        set_field_string(L, -1, "id", &profile.id);
        set_field_string(L, -1, "login", &profile.login);
        set_field_string(L, -1, "display_name", &profile.display_name);
        set_field_string(L, -1, "description", &profile.description);
        if let Some(created_at) = profile.created_at.as_ref() {
            set_field_string(L, -1, "created_at", created_at);
        }
        if let Some(avatar_url) = profile.avatar_url.as_ref() {
            set_field_string(L, -1, "avatar_url", avatar_url);
        }
        if let Some(followers) = profile.followers {
            set_field_int(L, -1, "followers", followers as i64);
        }
        set_field_bool(L, -1, "is_partner", profile.is_partner);
        set_field_bool(L, -1, "is_affiliate", profile.is_affiliate);
        if let Some(pronouns) = profile.pronouns.as_ref() {
            set_field_string(L, -1, "pronouns", pronouns);
        }
        if let Some(followed_at) = profile.followed_at.as_ref() {
            set_field_string(L, -1, "followed_at", followed_at);
        }
        if let Some(chat_color) = profile.chat_color.as_ref() {
            set_field_string(L, -1, "chat_color", chat_color);
        }
        set_field_bool(L, -1, "is_live", profile.is_live);
        if let Some(stream_title) = profile.stream_title.as_ref() {
            set_field_string(L, -1, "stream_title", stream_title);
        }
        if let Some(stream_game) = profile.stream_game.as_ref() {
            set_field_string(L, -1, "stream_game", stream_game);
        }
        if let Some(stream_viewers) = profile.stream_viewers {
            set_field_int(L, -1, "stream_viewers", stream_viewers as i64);
        }
        if let Some(last_broadcast_at) = profile.last_broadcast_at.as_ref() {
            set_field_string(L, -1, "last_broadcast_at", last_broadcast_at);
        }
        set_field_bool(L, -1, "is_banned", profile.is_banned);
        if let Some(ban_reason) = profile.ban_reason.as_ref() {
            set_field_string(L, -1, "ban_reason", ban_reason);
        }
    }
}

fn push_system_notice_table(L: *mut lua_State, notice: &SystemNotice) {
    unsafe {
        lua_createtable(L, 0, 0);
        if let Some(channel) = notice.channel.as_ref() {
            push_channel_table(L, channel);
            lua_setfield(L, -2, cstring("channel").as_ptr());
        }
        set_field_string(L, -1, "text", &notice.text);
        set_field_string(L, -1, "timestamp", &notice.timestamp.to_rfc3339());
    }
}

fn push_auto_mod_item_table(L: *mut lua_State, item: &AutoModQueueItem) {
    unsafe {
        lua_createtable(L, 0, 0);
        set_field_string(L, -1, "message_id", &item.message_id);
        set_field_string(L, -1, "sender_user_id", &item.sender_user_id);
        set_field_string(L, -1, "sender_login", &item.sender_login);
        set_field_string(L, -1, "text", &item.text);
        if let Some(reason) = item.reason.as_ref() {
            set_field_string(L, -1, "reason", reason);
        }
    }
}

fn push_unban_request_table(L: *mut lua_State, request: &UnbanRequestItem) {
    unsafe {
        lua_createtable(L, 0, 0);
        set_field_string(L, -1, "request_id", &request.request_id);
        set_field_string(L, -1, "user_id", &request.user_id);
        set_field_string(L, -1, "user_login", &request.user_login);
        if let Some(text) = request.text.as_ref() {
            set_field_string(L, -1, "text", text);
        }
        if let Some(created_at) = request.created_at.as_ref() {
            set_field_string(L, -1, "created_at", created_at);
        }
        if let Some(status) = request.status.as_ref() {
            set_field_string(L, -1, "status", status);
        }
    }
}

fn push_ivr_log_entry_table(L: *mut lua_State, entry: &IvrLogEntry) {
    unsafe {
        lua_createtable(L, 0, 0);
        set_field_string(L, -1, "text", &entry.text);
        set_field_string(L, -1, "timestamp", &entry.timestamp);
        set_field_string(L, -1, "display_name", &entry.display_name);
        set_field_int(L, -1, "msg_type", entry.msg_type as i64);
    }
}

fn push_link_preview_table(L: *mut lua_State, preview: &LinkPreview) {
    unsafe {
        lua_createtable(L, 0, 0);
        if let Some(title) = preview.title.as_ref() {
            set_field_string(L, -1, "title", title);
        }
        if let Some(description) = preview.description.as_ref() {
            set_field_string(L, -1, "description", description);
        }
        if let Some(thumbnail_url) = preview.thumbnail_url.as_ref() {
            set_field_string(L, -1, "thumbnail_url", thumbnail_url);
        }
        if let Some(site_name) = preview.site_name.as_ref() {
            set_field_string(L, -1, "site_name", site_name);
        }
        set_field_bool(L, -1, "fetched", preview.fetched);
    }
}

fn push_path_string(L: *mut lua_State, path: &Path) {
    let s = normalize_lua_path(&path.to_string_lossy());
    unsafe {
        lua_pushstring(L, cstring(&s).as_ptr());
    }
}

fn normalize_lua_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn set_field_string(L: *mut lua_State, idx: c_int, key: &str, value: &str) {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_pushstring(L, cstring(value).as_ptr());
        lua_setfield(L, idx, cstring(key).as_ptr());
    }
}

fn set_field_int(L: *mut lua_State, idx: c_int, key: &str, value: i64) {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_pushinteger(L, value as lua_Integer);
        lua_setfield(L, idx, cstring(key).as_ptr());
    }
}

fn set_field_bool(L: *mut lua_State, idx: c_int, key: &str, value: bool) {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_pushboolean(L, if value { 1 } else { 0 });
        lua_setfield(L, idx, cstring(key).as_ptr());
    }
}

fn set_field_number(L: *mut lua_State, idx: c_int, key: &str, value: f64) {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_pushnumber(L, value as lua_Number);
        lua_setfield(L, idx, cstring(key).as_ptr());
    }
}

fn set_list_string(L: *mut lua_State, idx: c_int, pos: lua_Integer, value: &str) {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_pushstring(L, cstring(value).as_ptr());
        lua_seti(L, idx, pos);
    }
}

fn lua_table_string_list(L: *mut lua_State, idx: c_int, field: &str) -> Vec<String> {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        let mut out = Vec::new();
        if lua_type(L, -1) == LUA_TTABLE {
            let len = lua_rawlen(L, -1);
            for i in 1..=len {
                lua_geti(L, -1, i as lua_Integer);
                if let Some(s) = lua_value_string(L, -1) {
                    out.push(s);
                }
                lua_pop(L, 1);
            }
        }
        lua_pop(L, 1);
        out
    }
}

fn lua_array_string_list(L: *mut lua_State, idx: c_int) -> Vec<String> {
    unsafe {
        let idx = lua_absindex(L, idx);
        let mut out = Vec::new();
        if lua_type(L, idx) == LUA_TTABLE {
            let len = lua_rawlen(L, idx);
            for i in 1..=len {
                lua_geti(L, idx, i as lua_Integer);
                if let Some(s) = lua_value_string(L, -1) {
                    out.push(s);
                }
                lua_pop(L, 1);
            }
        }
        out
    }
}

fn lua_table_bool(L: *mut lua_State, idx: c_int, field: &str, default: bool) -> bool {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        let value = lua_value_bool(L, -1).unwrap_or(default);
        lua_pop(L, 1);
        value
    }
}

fn lua_table_int(L: *mut lua_State, idx: c_int, field: &str, default: i64) -> i64 {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        let value = lua_value_int(L, -1).unwrap_or(default);
        lua_pop(L, 1);
        value
    }
}

fn lua_table_string(L: *mut lua_State, idx: c_int, field: &str) -> Option<String> {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        let value = lua_value_string(L, -1);
        lua_pop(L, 1);
        value
    }
}

fn lua_table_string_strict(L: *mut lua_State, idx: c_int, field: &str) -> Option<String> {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        let value = if lua_type(L, -1) == LUA_TSTRING {
            lua_value_string(L, -1)
        } else {
            None
        };
        lua_pop(L, 1);
        value
    }
}

fn lua_table_string_value(L: *mut lua_State, idx: c_int, field: &str, default: &str) -> String {
    lua_table_string(L, idx, field).unwrap_or_else(|| default.to_owned())
}

fn lua_table_reply_info(L: *mut lua_State, idx: c_int, field: &str) -> Option<ReplyInfo> {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        if lua_type(L, -1) != LUA_TTABLE {
            lua_pop(L, 1);
            return None;
        }
        let parent_msg_id = lua_table_string_strict(L, -1, "parent_msg_id");
        let parent_user_login = lua_table_string_strict(L, -1, "parent_user_login");
        let parent_display_name = lua_table_string_strict(L, -1, "parent_display_name");
        let parent_msg_body = lua_table_string_strict(L, -1, "parent_msg_body");
        lua_pop(L, 1);
        Some(ReplyInfo {
            parent_msg_id: parent_msg_id?,
            parent_user_login: parent_user_login?,
            parent_display_name: parent_display_name?,
            parent_msg_body: parent_msg_body?,
        })
    }
}

fn lua_table_usize(L: *mut lua_State, idx: c_int, field: &str, default: usize) -> usize {
    lua_table_int(L, idx, field, default as i64)
        .max(0)
        .try_into()
        .unwrap_or(default)
}

fn lua_table_keyed_string_list(L: *mut lua_State, idx: c_int, field: &str) -> Vec<String> {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        let mut out = Vec::new();
        if lua_type(L, -1) == LUA_TTABLE {
            let len = lua_rawlen(L, -1);
            for i in 1..=len {
                lua_geti(L, -1, i as lua_Integer);
                if let Some(s) = lua_value_string(L, -1) {
                    out.push(s);
                }
                lua_pop(L, 1);
            }
        }
        lua_pop(L, 1);
        out
    }
}

fn lua_table_usage_counts(L: *mut lua_State, idx: c_int, field: &str) -> Vec<(String, u32)> {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        let mut out = Vec::new();
        if lua_type(L, -1) == LUA_TTABLE {
            let len = lua_rawlen(L, -1);
            for i in 1..=len {
                lua_geti(L, -1, i as lua_Integer);
                if lua_type(L, -1) == LUA_TTABLE {
                    let name = lua_table_string(L, -1, "name")
                        .or_else(|| lua_table_string(L, -1, "command"));
                    let count = lua_table_int(L, -1, "count", 0).max(0) as u32;
                    if let Some(name) = name {
                        let normalized = name.trim().trim_start_matches('/').to_ascii_lowercase();
                        if !normalized.is_empty() {
                            out.push((normalized, count));
                        }
                    }
                }
                lua_pop(L, 1);
            }
        }
        lua_pop(L, 1);
        out
    }
}

fn current_plugin_index(L: *mut lua_State) -> Option<usize> {
    let state = L as usize;
    plugin_state_index()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .get(&state)
        .copied()
}

fn normalize_command_name(name: &str) -> String {
    name.trim().trim_start_matches('/').to_ascii_lowercase()
}

fn lua_value_bool(L: *mut lua_State, idx: c_int) -> Option<bool> {
    unsafe {
        match lua_type(L, idx) {
            LUA_TBOOLEAN => Some(lua_toboolean(L, idx) != 0),
            LUA_TNUMBER => Some(lua_value_int(L, idx).unwrap_or(0) != 0),
            _ => None,
        }
    }
}

fn lua_value_int(L: *mut lua_State, idx: c_int) -> Option<i64> {
    unsafe {
        let mut isnum = 0;
        let out = lua_tointegerx(L, idx, &mut isnum);
        if isnum != 0 {
            Some(out as i64)
        } else {
            None
        }
    }
}

fn lua_value_number(L: *mut lua_State, idx: c_int) -> Option<f64> {
    unsafe {
        let mut isnum = 0;
        let out = lua_tonumberx(L, idx, &mut isnum);
        if isnum != 0 {
            Some(out)
        } else {
            None
        }
    }
}

fn lua_value_string(L: *mut lua_State, idx: c_int) -> Option<String> {
    unsafe {
        match lua_type(L, idx) {
            LUA_TSTRING => {
                let ptr = lua_tolstring(L, idx, std::ptr::null_mut());
                if ptr.is_null() {
                    None
                } else {
                    Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
                }
            }
            LUA_TNUMBER => {
                let mut isnum = 0;
                let i = lua_tointegerx(L, idx, &mut isnum);
                if isnum != 0 {
                    return Some(i.to_string());
                }
                let mut isnumf = 0;
                let n = lua_tonumberx(L, idx, &mut isnumf);
                if isnumf != 0 {
                    return Some(n.to_string());
                }
                None
            }
            LUA_TBOOLEAN => Some((lua_toboolean(L, idx) != 0).to_string()),
            LUA_TNIL => None,
            _ => None,
        }
    }
}

fn lua_value_debug_string(L: *mut lua_State, idx: c_int) -> String {
    if let Some(s) = lua_value_string(L, idx) {
        return s;
    }
    unsafe {
        match lua_type(L, idx) {
            LUA_TNIL => "nil".to_owned(),
            LUA_TTABLE => "table".to_owned(),
            LUA_TFUNCTION => "function".to_owned(),
            other => {
                let name = lua_typename(L, other);
                if name.is_null() {
                    "<value>".to_owned()
                } else {
                    CStr::from_ptr(name).to_string_lossy().into_owned()
                }
            }
        }
    }
}

fn channel_from_value(L: *mut lua_State, idx: c_int) -> Option<ChannelId> {
    unsafe {
        let idx = lua_absindex(L, idx);
        if let Some(name) = lua_value_string(L, idx) {
            return ChannelId::parse_user_input(&name).or_else(|| Some(ChannelId::new(name)));
        }
        if lua_type(L, idx) == LUA_TTABLE {
            for key in ["id", "name", "display_name"] {
                lua_getfield(L, idx, cstring(key).as_ptr());
                let out = lua_value_string(L, -1).and_then(|name| {
                    ChannelId::parse_user_input(&name).or_else(|| Some(ChannelId::new(name)))
                });
                lua_pop(L, 1);
                if out.is_some() {
                    return out;
                }
            }
        }
        None
    }
}

fn is_supported_ui_widget_kind(kind: &str) -> bool {
    matches!(
        kind,
        "column"
            | "row"
            | "group"
            | "card"
            | "grid"
            | "scroll"
            | "separator"
            | "spacer"
            | "collapsible"
            | "text"
            | "heading"
            | "label"
            | "badge"
            | "image"
            | "progress"
            | "button"
            | "icon_button"
            | "link_button"
            | "text_input"
            | "text_area"
            | "password_input"
            | "checkbox"
            | "toggle"
            | "radio_group"
            | "select"
            | "slider"
            | "list"
            | "table"
    )
}

fn normalize_ui_widget_kind(kind: &str) -> Option<String> {
    let normalized = kind.trim().to_ascii_lowercase();
    if is_supported_ui_widget_kind(&normalized) {
        Some(normalized)
    } else {
        None
    }
}

fn lua_table_number(L: *mut lua_State, idx: c_int, field: &str) -> Option<f64> {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        let value = lua_value_number(L, -1);
        lua_pop(L, 1);
        value
    }
}

fn lua_table_bool_opt(L: *mut lua_State, idx: c_int, field: &str) -> Option<bool> {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        let value = lua_value_bool(L, -1);
        lua_pop(L, 1);
        value
    }
}

fn lua_value_ui_value(L: *mut lua_State, idx: c_int) -> Option<PluginUiValue> {
    unsafe {
        match lua_type(L, idx) {
            LUA_TSTRING => lua_value_string(L, idx).map(PluginUiValue::String),
            LUA_TBOOLEAN => lua_value_bool(L, idx).map(PluginUiValue::Bool),
            LUA_TNUMBER => lua_value_number(L, idx).map(PluginUiValue::Number),
            LUA_TTABLE => {
                let idx = lua_absindex(L, idx);
                let len = lua_rawlen(L, idx);
                let mut values = Vec::new();
                for i in 1..=len {
                    lua_geti(L, idx, i as lua_Integer);
                    if let Some(value) = lua_value_string(L, -1) {
                        values.push(value);
                    }
                    lua_pop(L, 1);
                }
                Some(PluginUiValue::Strings(values))
            }
            _ => None,
        }
    }
}

fn lua_table_ui_value(L: *mut lua_State, idx: c_int, field: &str) -> Option<PluginUiValue> {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        let value = lua_value_ui_value(L, -1);
        lua_pop(L, 1);
        value
    }
}

fn lua_table_ui_style(L: *mut lua_State, idx: c_int, field: &str) -> PluginUiStyle {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        if lua_type(L, -1) != LUA_TTABLE {
            lua_pop(L, 1);
            return PluginUiStyle::default();
        }
        let style = PluginUiStyle {
            visible: lua_table_bool_opt(L, -1, "visible"),
            enabled: lua_table_bool_opt(L, -1, "enabled"),
            width: lua_table_number(L, -1, "width").map(|v| v as f32),
            height: lua_table_number(L, -1, "height").map(|v| v as f32),
            min_width: lua_table_number(L, -1, "min_width").map(|v| v as f32),
            min_height: lua_table_number(L, -1, "min_height").map(|v| v as f32),
            max_width: lua_table_number(L, -1, "max_width").map(|v| v as f32),
            max_height: lua_table_number(L, -1, "max_height").map(|v| v as f32),
            padding: lua_table_number(L, -1, "padding").map(|v| v as f32),
            align: lua_table_string_strict(L, -1, "align"),
            text_role: lua_table_string_strict(L, -1, "text_role"),
            emphasis: lua_table_string_strict(L, -1, "emphasis"),
            border_color: lua_table_string_strict(L, -1, "border_color"),
            fill_color: lua_table_string_strict(L, -1, "fill_color"),
            severity: lua_table_string_strict(L, -1, "severity"),
            icon: lua_table_string_strict(L, -1, "icon"),
            image_url: lua_table_string_strict(L, -1, "image_url"),
        };
        lua_pop(L, 1);
        style
    }
}

fn lua_table_ui_choices(L: *mut lua_State, idx: c_int, field: &str) -> Vec<PluginUiChoice> {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        let mut out = Vec::new();
        if lua_type(L, -1) == LUA_TTABLE {
            let len = lua_rawlen(L, -1);
            for i in 1..=len {
                lua_geti(L, -1, i as lua_Integer);
                if lua_type(L, -1) == LUA_TTABLE {
                    let label = lua_table_string_strict(L, -1, "label");
                    let value = lua_table_string_strict(L, -1, "value");
                    if let (Some(label), Some(value)) = (label, value) {
                        out.push(PluginUiChoice {
                            label,
                            value,
                            description: lua_table_string_strict(L, -1, "description"),
                        });
                    }
                }
                lua_pop(L, 1);
            }
        }
        lua_pop(L, 1);
        out
    }
}

fn lua_table_ui_columns(L: *mut lua_State, idx: c_int, field: &str) -> Vec<PluginUiTableColumn> {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        let mut out = Vec::new();
        if lua_type(L, -1) == LUA_TTABLE {
            let len = lua_rawlen(L, -1);
            for i in 1..=len {
                lua_geti(L, -1, i as lua_Integer);
                if lua_type(L, -1) == LUA_TTABLE {
                    let id = lua_table_string_strict(L, -1, "id")
                        .or_else(|| lua_table_string_strict(L, -1, "title"));
                    let title = lua_table_string_strict(L, -1, "title")
                        .or_else(|| lua_table_string_strict(L, -1, "id"));
                    if let (Some(id), Some(title)) = (id, title) {
                        out.push(PluginUiTableColumn {
                            id,
                            title,
                            align: lua_table_string_strict(L, -1, "align"),
                        });
                    }
                }
                lua_pop(L, 1);
            }
        }
        lua_pop(L, 1);
        out
    }
}

fn lua_table_ui_rows(L: *mut lua_State, idx: c_int, field: &str) -> Vec<Vec<PluginUiValue>> {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        let mut out = Vec::new();
        if lua_type(L, -1) == LUA_TTABLE {
            let len = lua_rawlen(L, -1);
            for i in 1..=len {
                lua_geti(L, -1, i as lua_Integer);
                if lua_type(L, -1) == LUA_TTABLE {
                    let row_idx = lua_absindex(L, -1);
                    let row_len = lua_rawlen(L, row_idx);
                    let mut row = Vec::new();
                    for j in 1..=row_len {
                        lua_geti(L, row_idx, j as lua_Integer);
                        if let Some(value) = lua_value_ui_value(L, -1) {
                            row.push(value);
                        }
                        lua_pop(L, 1);
                    }
                    out.push(row);
                }
                lua_pop(L, 1);
            }
        }
        lua_pop(L, 1);
        out
    }
}

fn lua_table_ui_items(
    L: *mut lua_State,
    idx: c_int,
    field: &str,
) -> Vec<crust_core::plugins::PluginUiListItem> {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        let mut out = Vec::new();
        if lua_type(L, -1) == LUA_TTABLE {
            let len = lua_rawlen(L, -1);
            for i in 1..=len {
                lua_geti(L, -1, i as lua_Integer);
                match lua_type(L, -1) {
                    LUA_TTABLE => {
                        if let Some(label) = lua_table_string_strict(L, -1, "label") {
                            out.push(crust_core::plugins::PluginUiListItem {
                                label,
                                value: lua_table_string_strict(L, -1, "value"),
                                note: lua_table_string_strict(L, -1, "note"),
                            });
                        }
                    }
                    LUA_TSTRING | LUA_TNUMBER | LUA_TBOOLEAN => {
                        if let Some(label) = lua_value_string(L, -1) {
                            out.push(crust_core::plugins::PluginUiListItem {
                                label,
                                value: None,
                                note: None,
                            });
                        }
                    }
                    _ => {}
                }
                lua_pop(L, 1);
            }
        }
        lua_pop(L, 1);
        out
    }
}

fn lua_table_ui_children_from_field(
    L: *mut lua_State,
    idx: c_int,
    field: &str,
) -> Vec<PluginUiWidget> {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_getfield(L, idx, cstring(field).as_ptr());
        let mut out = Vec::new();
        if lua_type(L, -1) == LUA_TTABLE {
            let len = lua_rawlen(L, -1);
            for i in 1..=len {
                lua_geti(L, -1, i as lua_Integer);
                if let Some(widget) = lua_value_ui_widget(L, -1) {
                    out.push(widget);
                }
                lua_pop(L, 1);
            }
        }
        lua_pop(L, 1);
        out
    }
}

fn lua_table_ui_children(L: *mut lua_State, idx: c_int) -> Vec<PluginUiWidget> {
    for field in ["children", "widgets", "body"] {
        let children = lua_table_ui_children_from_field(L, idx, field);
        if !children.is_empty() {
            return children;
        }
    }
    Vec::new()
}

fn lua_value_ui_widget(L: *mut lua_State, idx: c_int) -> Option<PluginUiWidget> {
    unsafe {
        let idx = lua_absindex(L, idx);
        if lua_type(L, idx) != LUA_TTABLE {
            return None;
        }
        let kind = lua_table_string_strict(L, idx, "type")
            .or_else(|| lua_table_string_strict(L, idx, "kind"))
            .and_then(|value| normalize_ui_widget_kind(&value))?;
        Some(PluginUiWidget {
            kind,
            id: lua_table_string_strict(L, idx, "id"),
            title: lua_table_string_strict(L, idx, "title"),
            text: lua_table_string_strict(L, idx, "text")
                .or_else(|| lua_table_string_strict(L, idx, "label")),
            action: lua_table_string_strict(L, idx, "action"),
            url: lua_table_string_strict(L, idx, "url"),
            placeholder: lua_table_string_strict(L, idx, "placeholder"),
            value: lua_table_ui_value(L, idx, "value"),
            progress: lua_table_number(L, idx, "progress").map(|v| v as f32),
            min: lua_table_number(L, idx, "min"),
            max: lua_table_number(L, idx, "max"),
            step: lua_table_number(L, idx, "step"),
            rows: lua_table_ui_rows(L, idx, "rows"),
            children: lua_table_ui_children(L, idx),
            options: lua_table_ui_choices(L, idx, "options"),
            items: lua_table_ui_items(L, idx, "items"),
            columns: lua_table_ui_columns(L, idx, "columns"),
            form_key: lua_table_string_strict(L, idx, "form_key"),
            host_form: lua_table_bool(L, idx, "host_form", false),
            submit: lua_table_bool(L, idx, "submit", false),
            open: lua_table_bool_opt(L, idx, "open"),
            style: lua_table_ui_style(L, idx, "style"),
        })
    }
}

fn lua_table_ui_window_spec(
    L: *mut lua_State,
    idx: c_int,
    override_id: Option<&str>,
) -> Option<PluginUiWindowSpec> {
    unsafe {
        let idx = lua_absindex(L, idx);
        if lua_type(L, idx) != LUA_TTABLE {
            return None;
        }
        let id = override_id
            .map(str::to_owned)
            .or_else(|| lua_table_string_strict(L, idx, "id"))?;
        let title = lua_table_string_strict(L, idx, "title").unwrap_or_else(|| id.clone());
        Some(PluginUiWindowSpec {
            id,
            title,
            open: lua_table_bool(L, idx, "open", true),
            resizable: lua_table_bool(L, idx, "resizable", true),
            scroll: lua_table_bool(L, idx, "scroll", false),
            default_width: lua_table_number(L, idx, "default_width").map(|v| v as f32),
            default_height: lua_table_number(L, idx, "default_height").map(|v| v as f32),
            min_width: lua_table_number(L, idx, "min_width").map(|v| v as f32),
            min_height: lua_table_number(L, idx, "min_height").map(|v| v as f32),
            max_width: lua_table_number(L, idx, "max_width").map(|v| v as f32),
            max_height: lua_table_number(L, idx, "max_height").map(|v| v as f32),
            children: lua_table_ui_children(L, idx),
            style: lua_table_ui_style(L, idx, "style"),
        })
    }
}

fn lua_table_ui_settings_page_spec(
    L: *mut lua_State,
    idx: c_int,
    override_id: Option<&str>,
) -> Option<PluginUiSettingsPageSpec> {
    unsafe {
        let idx = lua_absindex(L, idx);
        if lua_type(L, idx) != LUA_TTABLE {
            return None;
        }
        let id = override_id
            .map(str::to_owned)
            .or_else(|| lua_table_string_strict(L, idx, "id"))?;
        let title = lua_table_string_strict(L, idx, "title").unwrap_or_else(|| id.clone());
        Some(PluginUiSettingsPageSpec {
            id,
            title,
            summary: lua_table_string_strict(L, idx, "summary"),
            children: lua_table_ui_children(L, idx),
            style: lua_table_ui_style(L, idx, "style"),
        })
    }
}

fn plugin_ui_value_map_to_lua_table(L: *mut lua_State, values: &BTreeMap<String, PluginUiValue>) {
    unsafe {
        lua_createtable(L, 0, 0);
        let idx = lua_absindex(L, -1);
        for (key, value) in values {
            push_plugin_ui_value(L, value);
            lua_setfield(L, idx, cstring(key).as_ptr());
        }
    }
}

fn lua_host_slot_from_string(value: &str) -> Option<PluginUiHostSlot> {
    match value.trim().to_ascii_lowercase().as_str() {
        "settings.integrations" => Some(PluginUiHostSlot::SettingsIntegrations),
        "settings.appearance" => Some(PluginUiHostSlot::SettingsAppearance),
        "settings.chat" => Some(PluginUiHostSlot::SettingsChat),
        "sidebar.top" => Some(PluginUiHostSlot::SidebarTop),
        "channel_header" => Some(PluginUiHostSlot::ChannelHeader),
        _ => None,
    }
}

fn lua_table_ui_host_panel_spec(
    L: *mut lua_State,
    idx: c_int,
    override_id: Option<&str>,
) -> Option<PluginUiHostPanelSpec> {
    unsafe {
        let idx = lua_absindex(L, idx);
        if lua_type(L, idx) != LUA_TTABLE {
            return None;
        }
        let id = override_id
            .map(str::to_owned)
            .or_else(|| lua_table_string_strict(L, idx, "id"))?;
        let slot = lua_table_string_strict(L, idx, "slot")
            .and_then(|value| lua_host_slot_from_string(&value))?;
        Some(PluginUiHostPanelSpec {
            id,
            slot,
            title: lua_table_string_strict(L, idx, "title"),
            summary: lua_table_string_strict(L, idx, "summary"),
            order: lua_table_int(L, idx, "order", 0).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            children: lua_table_ui_children(L, idx),
            style: lua_table_ui_style(L, idx, "style"),
        })
    }
}

fn push_plugin_ui_value(L: *mut lua_State, value: &PluginUiValue) {
    unsafe {
        match value {
            PluginUiValue::String(value) => {
                lua_pushstring(L, cstring(value).as_ptr());
            }
            PluginUiValue::Bool(value) => {
                lua_pushboolean(L, if *value { 1 } else { 0 });
            }
            PluginUiValue::Number(value) => {
                lua_pushnumber(L, *value as lua_Number);
            }
            PluginUiValue::Strings(values) => {
                lua_createtable(L, values.len() as c_int, 0);
                let idx = lua_absindex(L, -1);
                for (i, value) in values.iter().enumerate() {
                    lua_pushstring(L, cstring(value).as_ptr());
                    lua_seti(L, idx, (i + 1) as lua_Integer);
                }
            }
        }
    }
}

unsafe fn lua_error_text(L: *mut lua_State, _code: c_int) -> String {
    let msg = lua_value_string(L, -1).unwrap_or_else(|| "Lua error".to_owned());
    luaL_traceback(L, L, cstring(&msg).as_ptr(), 1);
    let trace = lua_value_string(L, -1).unwrap_or(msg);
    let top = lua_gettop(L);
    lua_settop(L, top - 2);
    trace
}

fn event_kind(event: &AppEvent) -> Option<PluginEventKind> {
    Some(match event {
        AppEvent::EmoteImageReady { .. } => PluginEventKind::EmoteImageReady,
        AppEvent::Authenticated { .. } => PluginEventKind::Authenticated,
        AppEvent::EmoteCatalogUpdated { .. } => PluginEventKind::EmoteCatalogUpdated,
        AppEvent::LoggedOut => PluginEventKind::LoggedOut,
        AppEvent::AccountListUpdated { .. } => PluginEventKind::AccountListUpdated,
        AppEvent::ChannelJoined { .. } => PluginEventKind::ChannelJoined,
        AppEvent::ChannelParted { .. } => PluginEventKind::ChannelParted,
        AppEvent::ChannelRedirected { .. } => PluginEventKind::ChannelRedirected,
        AppEvent::ConnectionStateChanged { .. } => PluginEventKind::ConnectionStateChanged,
        AppEvent::MessageReceived { .. } => PluginEventKind::MessageReceived,
        AppEvent::WhisperReceived { .. } => PluginEventKind::WhisperReceived,
        AppEvent::MessageDeleted { .. } => PluginEventKind::MessageDeleted,
        AppEvent::SystemNotice(_) => PluginEventKind::SystemNotice,
        AppEvent::Error { .. } => PluginEventKind::Error,
        AppEvent::HistoryLoaded { .. } => PluginEventKind::HistoryLoaded,
        AppEvent::UserProfileLoaded { .. } => PluginEventKind::UserProfileLoaded,
        AppEvent::UserProfileUnavailable { .. } => PluginEventKind::UserProfileUnavailable,
        AppEvent::StreamStatusUpdated { .. } => PluginEventKind::StreamStatusUpdated,
        AppEvent::IvrLogsLoaded { .. } => PluginEventKind::IvrLogsLoaded,
        AppEvent::IvrLogsFailed { .. } => PluginEventKind::IvrLogsFailed,
        AppEvent::ChannelEmotesLoaded { .. } => PluginEventKind::ChannelEmotesLoaded,
        AppEvent::BetaFeaturesUpdated { .. } => PluginEventKind::BetaFeaturesUpdated,
        AppEvent::ChatUiBehaviorUpdated { .. } => PluginEventKind::ChatUiBehaviorUpdated,
        AppEvent::GeneralSettingsUpdated { .. } => PluginEventKind::GeneralSettingsUpdated,
        AppEvent::SlashUsageCountsUpdated { .. } => PluginEventKind::SlashUsageCountsUpdated,
        AppEvent::EmotePickerPreferencesUpdated { .. } => {
            PluginEventKind::EmotePickerPreferencesUpdated
        }
        AppEvent::AppearanceSettingsUpdated { .. } => PluginEventKind::AppearanceSettingsUpdated,
        AppEvent::FontSettingsUpdated { .. } => PluginEventKind::FontSettingsUpdated,
        AppEvent::RestoreLastActiveChannel { .. } => PluginEventKind::RestoreLastActiveChannel,
        AppEvent::RoomStateUpdated { .. } => PluginEventKind::RoomStateUpdated,
        AppEvent::AutoModQueueAppend { .. } => PluginEventKind::AutoModQueueAppend,
        AppEvent::AutoModQueueRemove { .. } => PluginEventKind::AutoModQueueRemove,
        AppEvent::UnbanRequestsLoaded { .. } => PluginEventKind::UnbanRequestsLoaded,
        AppEvent::UnbanRequestsFailed { .. } => PluginEventKind::UnbanRequestsFailed,
        AppEvent::UnbanRequestUpsert { .. } => PluginEventKind::UnbanRequestUpsert,
        AppEvent::UnbanRequestResolved { .. } => PluginEventKind::UnbanRequestResolved,
        AppEvent::OpenModerationTools { .. } => PluginEventKind::OpenModerationTools,
        AppEvent::HighlightRulesUpdated { .. } => PluginEventKind::HighlightRulesUpdated,
        AppEvent::FilterRecordsUpdated { .. } => PluginEventKind::FilterRecordsUpdated,
        AppEvent::ModActionPresetsUpdated { .. } => PluginEventKind::ModActionPresetsUpdated,
        AppEvent::NicknamesUpdated { .. } => PluginEventKind::NicknamesUpdated,
        AppEvent::IgnoredUsersUpdated { .. } => PluginEventKind::IgnoredUsersUpdated,
        AppEvent::IgnoredPhrasesUpdated { .. } => PluginEventKind::IgnoredPhrasesUpdated,
        AppEvent::UserPronounsLoaded { .. } => PluginEventKind::UserPronounsLoaded,
        AppEvent::UsercardSettingsUpdated { .. } => PluginEventKind::UsercardSettingsUpdated,
        AppEvent::AuthExpired => PluginEventKind::AuthExpired,
        AppEvent::UpdaterSettingsUpdated { .. }
        | AppEvent::UpdateInstallStarted { .. }
        | AppEvent::UpdateInstallScheduled { .. }
        | AppEvent::UpdateInstallFailed { .. }
        | AppEvent::UpdateAvailable { .. }
        | AppEvent::UpdateCheckUpToDate { .. }
        | AppEvent::UpdateCheckFailed { .. } => PluginEventKind::Error,
        AppEvent::StreamerModeSettingsUpdated { .. }
        | AppEvent::StreamerModeActiveChanged { .. } => PluginEventKind::Error,
        AppEvent::SelfAvatarLoaded { .. } => PluginEventKind::SelfAvatarLoaded,
        AppEvent::LinkPreviewReady { .. } => PluginEventKind::LinkPreviewReady,
        AppEvent::SenderCosmeticsUpdated { .. } => PluginEventKind::SenderCosmeticsUpdated,
        AppEvent::IrcTopicChanged { .. } => PluginEventKind::IrcTopicChanged,
        AppEvent::UserStateUpdated { .. } => PluginEventKind::UserStateUpdated,
        AppEvent::UserMessagesCleared { .. } => PluginEventKind::UserMessagesCleared,
        AppEvent::LowTrustStatusUpdated { .. } => PluginEventKind::LowTrustStatusUpdated,
        AppEvent::ChannelMessagesCleared { .. } => PluginEventKind::ChannelMessagesCleared,
        AppEvent::ClearUserMessagesLocally { .. } => PluginEventKind::ClearUserMessagesLocally,
        AppEvent::ImagePrefetchQueued { .. } => PluginEventKind::ImagePrefetchQueued,
        AppEvent::PluginUiAction { .. } => PluginEventKind::PluginUiAction,
        AppEvent::PluginUiChange { .. } => PluginEventKind::PluginUiChange,
        AppEvent::PluginUiSubmit { .. } => PluginEventKind::PluginUiSubmit,
        AppEvent::PluginUiWindowClosed { .. } => PluginEventKind::PluginUiWindowClosed,
    })
}

fn plugin_target_name(event: &AppEvent) -> Option<String> {
    match event {
        AppEvent::PluginUiAction { plugin_name, .. }
        | AppEvent::PluginUiChange { plugin_name, .. }
        | AppEvent::PluginUiSubmit { plugin_name, .. }
        | AppEvent::PluginUiWindowClosed { plugin_name, .. } => Some(plugin_name.clone()),
        _ => None,
    }
}

unsafe fn make_event_table(L: *mut lua_State, event: &AppEvent) -> c_int {
    lua_createtable(L, 0, 0);
    set_field_string(
        L,
        -1,
        "type",
        event_kind_name(event_kind(event).unwrap_or(PluginEventKind::Error)),
    );

    match event {
        AppEvent::EmoteImageReady {
            uri,
            width,
            height,
            raw_bytes,
        } => {
            set_field_string(L, -1, "uri", uri);
            set_field_int(L, -1, "width", *width as i64);
            set_field_int(L, -1, "height", *height as i64);
            let raw_bytes_base64 = BASE64_STANDARD.encode(raw_bytes);
            set_field_string(L, -1, "raw_bytes_base64", &raw_bytes_base64);
        }
        AppEvent::ImagePrefetchQueued { count } => {
            set_field_int(L, -1, "count", *count as i64);
        }
        AppEvent::Authenticated { username, user_id } => {
            set_field_string(L, -1, "username", username);
            set_field_string(L, -1, "user_id", user_id);
        }
        AppEvent::EmoteCatalogUpdated { emotes } => {
            lua_createtable(L, emotes.len() as c_int, 0);
            for (idx, emote) in emotes.iter().enumerate() {
                push_emote_catalog_entry_table(L, emote);
                lua_seti(L, -2, (idx + 1) as lua_Integer);
            }
            lua_setfield(L, -2, cstring("emotes").as_ptr());
        }
        AppEvent::LoggedOut => {}
        AppEvent::AccountListUpdated {
            accounts,
            active,
            default,
        } => {
            lua_createtable(L, accounts.len() as c_int, 0);
            for (idx, account) in accounts.iter().enumerate() {
                set_list_string(L, -1, (idx + 1) as lua_Integer, account);
            }
            lua_setfield(L, -2, cstring("accounts").as_ptr());
            if let Some(active) = active.as_ref() {
                set_field_string(L, -1, "active", active);
            }
            if let Some(default) = default.as_ref() {
                set_field_string(L, -1, "default", default);
            }
        }
        AppEvent::ChannelJoined { channel }
        | AppEvent::ChannelParted { channel }
        | AppEvent::ChannelMessagesCleared { channel }
        | AppEvent::ClearUserMessagesLocally { channel, .. } => {
            push_channel_table(L, channel);
            lua_setfield(L, -2, cstring("channel").as_ptr());
        }
        AppEvent::ChannelRedirected {
            old_channel,
            new_channel,
        } => {
            push_channel_table(L, old_channel);
            lua_setfield(L, -2, cstring("old_channel").as_ptr());
            push_channel_table(L, new_channel);
            lua_setfield(L, -2, cstring("new_channel").as_ptr());
        }
        AppEvent::ConnectionStateChanged { state } => {
            set_field_string(L, -1, "state", &state.to_string());
        }
        AppEvent::MessageReceived { channel, message } => {
            push_channel_table(L, channel);
            lua_setfield(L, -2, cstring("channel").as_ptr());
            push_chat_message_table(L, message);
            lua_setfield(L, -2, cstring("message").as_ptr());
        }
        AppEvent::WhisperReceived {
            from_login,
            from_display_name,
            target_login,
            text,
            twitch_emotes,
            is_self,
            timestamp,
            is_history,
        } => {
            set_field_string(L, -1, "from_login", from_login);
            set_field_string(L, -1, "from_display_name", from_display_name);
            set_field_string(L, -1, "target_login", target_login);
            set_field_string(L, -1, "text", text);
            lua_createtable(L, twitch_emotes.len() as c_int, 0);
            for (idx, emote) in twitch_emotes.iter().enumerate() {
                push_twitch_emote_table(L, emote);
                lua_seti(L, -2, (idx + 1) as lua_Integer);
            }
            lua_setfield(L, -2, cstring("twitch_emotes").as_ptr());
            set_field_bool(L, -1, "is_self", *is_self);
            set_field_string(L, -1, "timestamp", &timestamp.to_rfc3339());
            set_field_bool(L, -1, "is_history", *is_history);
        }
        AppEvent::MessageDeleted { channel, server_id } => {
            push_channel_table(L, channel);
            lua_setfield(L, -2, cstring("channel").as_ptr());
            set_field_string(L, -1, "server_id", server_id);
        }
        AppEvent::UserMessagesCleared { channel, login } => {
            push_channel_table(L, channel);
            lua_setfield(L, -2, cstring("channel").as_ptr());
            set_field_string(L, -1, "login", login);
        }
        AppEvent::LowTrustStatusUpdated {
            channel,
            login,
            status,
        } => {
            push_channel_table(L, channel);
            lua_setfield(L, -2, cstring("channel").as_ptr());
            set_field_string(L, -1, "login", login);
            let s = match status {
                Some(crust_core::model::LowTrustStatus::Monitored) => "monitored",
                Some(crust_core::model::LowTrustStatus::Restricted) => "restricted",
                None => "none",
            };
            set_field_string(L, -1, "status", s);
        }
        AppEvent::SystemNotice(notice) => {
            push_system_notice_table(L, notice);
            lua_setfield(L, -2, cstring("notice").as_ptr());
        }
        AppEvent::Error { context, message } => {
            set_field_string(L, -1, "context", context);
            set_field_string(L, -1, "message", message);
        }
        AppEvent::HistoryLoaded { channel, messages } => {
            push_channel_table(L, channel);
            lua_setfield(L, -2, cstring("channel").as_ptr());
            lua_createtable(L, messages.len() as c_int, 0);
            for (idx, message) in messages.iter().enumerate() {
                push_chat_message_table(L, message);
                lua_seti(L, -2, (idx + 1) as lua_Integer);
            }
            lua_setfield(L, -2, cstring("messages").as_ptr());
        }
        AppEvent::UserProfileLoaded { profile } => {
            push_user_profile_table(L, profile);
            lua_setfield(L, -2, cstring("profile").as_ptr());
        }
        AppEvent::UserProfileUnavailable { login } => {
            set_field_string(L, -1, "login", login);
        }
        AppEvent::StreamStatusUpdated {
            login,
            is_live,
            title,
            game,
            viewers,
        } => {
            set_field_string(L, -1, "login", login);
            set_field_bool(L, -1, "is_live", *is_live);
            if let Some(title) = title.as_ref() {
                set_field_string(L, -1, "title", title);
            }
            if let Some(game) = game.as_ref() {
                set_field_string(L, -1, "game", game);
            }
            if let Some(viewers) = viewers {
                set_field_int(L, -1, "viewers", *viewers as i64);
            }
        }
        AppEvent::IvrLogsLoaded { username, messages } => {
            set_field_string(L, -1, "username", username);
            lua_createtable(L, messages.len() as c_int, 0);
            for (idx, entry) in messages.iter().enumerate() {
                push_ivr_log_entry_table(L, entry);
                lua_seti(L, -2, (idx + 1) as lua_Integer);
            }
            lua_setfield(L, -2, cstring("messages").as_ptr());
        }
        AppEvent::IvrLogsFailed { username, error } => {
            set_field_string(L, -1, "username", username);
            set_field_string(L, -1, "error", error);
        }
        AppEvent::ChannelEmotesLoaded { channel, count } => {
            push_channel_table(L, channel);
            lua_setfield(L, -2, cstring("channel").as_ptr());
            set_field_int(L, -1, "count", *count as i64);
        }
        AppEvent::BetaFeaturesUpdated {
            kick_enabled,
            irc_enabled,
            irc_nickserv_user,
            irc_nickserv_pass,
            always_on_top,
        } => {
            set_field_bool(L, -1, "kick_enabled", *kick_enabled);
            set_field_bool(L, -1, "irc_enabled", *irc_enabled);
            set_field_string(L, -1, "irc_nickserv_user", irc_nickserv_user);
            set_field_string(L, -1, "irc_nickserv_pass", irc_nickserv_pass);
            set_field_bool(L, -1, "always_on_top", *always_on_top);
        }
        AppEvent::ChatUiBehaviorUpdated {
            prevent_overlong_twitch_messages,
            collapse_long_messages,
            collapse_long_message_lines,
            animations_when_focused,
        } => {
            set_field_bool(
                L,
                -1,
                "prevent_overlong_twitch_messages",
                *prevent_overlong_twitch_messages,
            );
            set_field_bool(L, -1, "collapse_long_messages", *collapse_long_messages);
            set_field_int(
                L,
                -1,
                "collapse_long_message_lines",
                *collapse_long_message_lines as i64,
            );
            set_field_bool(L, -1, "animations_when_focused", *animations_when_focused);
        }
        AppEvent::GeneralSettingsUpdated {
            show_timestamps,
            show_timestamp_seconds,
            use_24h_timestamps,
            local_log_indexing_enabled,
            auto_join,
            highlights,
            ignores,
            desktop_notifications_enabled,
        } => {
            set_field_bool(L, -1, "show_timestamps", *show_timestamps);
            set_field_bool(L, -1, "show_timestamp_seconds", *show_timestamp_seconds);
            set_field_bool(L, -1, "use_24h_timestamps", *use_24h_timestamps);
            set_field_bool(
                L,
                -1,
                "local_log_indexing_enabled",
                *local_log_indexing_enabled,
            );
            push_string_array(L, auto_join);
            lua_setfield(L, -2, cstring("auto_join").as_ptr());
            push_string_array(L, highlights);
            lua_setfield(L, -2, cstring("highlights").as_ptr());
            push_string_array(L, ignores);
            lua_setfield(L, -2, cstring("ignores").as_ptr());
            set_field_bool(
                L,
                -1,
                "desktop_notifications_enabled",
                *desktop_notifications_enabled,
            );
        }
        AppEvent::SlashUsageCountsUpdated { usage_counts } => {
            lua_createtable(L, usage_counts.len() as c_int, 0);
            for (idx, (name, count)) in usage_counts.iter().enumerate() {
                lua_createtable(L, 0, 0);
                set_field_string(L, -1, "name", name);
                set_field_int(L, -1, "count", *count as i64);
                lua_seti(L, -2, (idx + 1) as lua_Integer);
            }
            lua_setfield(L, -2, cstring("usage_counts").as_ptr());
        }
        AppEvent::EmotePickerPreferencesUpdated {
            favorites,
            recent,
            provider_boost,
        } => {
            push_string_array(L, favorites);
            lua_setfield(L, -2, cstring("favorites").as_ptr());
            push_string_array(L, recent);
            lua_setfield(L, -2, cstring("recent").as_ptr());
            if let Some(provider_boost) = provider_boost.as_ref() {
                set_field_string(L, -1, "provider_boost", provider_boost);
            }
        }
        AppEvent::FontSettingsUpdated {
            chat_font_size,
            ui_font_size,
            topbar_font_size,
            tabs_font_size,
            timestamps_font_size,
            pills_font_size,
        } => {
            set_field_number(L, -1, "chat_font_size", *chat_font_size as f64);
            set_field_number(L, -1, "ui_font_size", *ui_font_size as f64);
            set_field_number(L, -1, "topbar_font_size", *topbar_font_size as f64);
            set_field_number(L, -1, "tabs_font_size", *tabs_font_size as f64);
            set_field_number(L, -1, "timestamps_font_size", *timestamps_font_size as f64);
            set_field_number(L, -1, "pills_font_size", *pills_font_size as f64);
        }
        AppEvent::RestoreLastActiveChannel { channel } => {
            set_field_string(L, -1, "channel", channel);
        }
        AppEvent::AppearanceSettingsUpdated {
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
            set_field_string(L, -1, "channel_layout", channel_layout);
            set_field_bool(L, -1, "sidebar_visible", *sidebar_visible);
            set_field_bool(L, -1, "analytics_visible", *analytics_visible);
            set_field_bool(L, -1, "irc_status_visible", *irc_status_visible);
            set_field_string(L, -1, "tab_style", tab_style);
            set_field_bool(L, -1, "show_tab_close_buttons", *show_tab_close_buttons);
            set_field_bool(L, -1, "show_tab_live_indicators", *show_tab_live_indicators);
            set_field_bool(L, -1, "split_header_show_title", *split_header_show_title);
            set_field_bool(L, -1, "split_header_show_game", *split_header_show_game);
            set_field_bool(
                L,
                -1,
                "split_header_show_viewer_count",
                *split_header_show_viewer_count,
            );
        }
        AppEvent::RoomStateUpdated {
            channel,
            emote_only,
            followers_only,
            slow,
            subs_only,
            r9k,
        } => {
            push_channel_table(L, channel);
            lua_setfield(L, -2, cstring("channel").as_ptr());
            if let Some(v) = emote_only {
                set_field_bool(L, -1, "emote_only", *v);
            }
            if let Some(v) = followers_only {
                set_field_int(L, -1, "followers_only", *v as i64);
            }
            if let Some(v) = slow {
                set_field_int(L, -1, "slow", *v as i64);
            }
            if let Some(v) = subs_only {
                set_field_bool(L, -1, "subs_only", *v);
            }
            if let Some(v) = r9k {
                set_field_bool(L, -1, "r9k", *v);
            }
        }
        AppEvent::AutoModQueueAppend { channel, item } => {
            push_channel_table(L, channel);
            lua_setfield(L, -2, cstring("channel").as_ptr());
            push_auto_mod_item_table(L, item);
            lua_setfield(L, -2, cstring("item").as_ptr());
        }
        AppEvent::AutoModQueueRemove {
            channel,
            message_id,
            action,
        } => {
            push_channel_table(L, channel);
            lua_setfield(L, -2, cstring("channel").as_ptr());
            set_field_string(L, -1, "message_id", message_id);
            if let Some(action) = action.as_ref() {
                set_field_string(L, -1, "action", action);
            }
        }
        AppEvent::UnbanRequestsLoaded { channel, requests } => {
            push_channel_table(L, channel);
            lua_setfield(L, -2, cstring("channel").as_ptr());
            lua_createtable(L, requests.len() as c_int, 0);
            for (idx, request) in requests.iter().enumerate() {
                push_unban_request_table(L, request);
                lua_seti(L, -2, (idx + 1) as lua_Integer);
            }
            lua_setfield(L, -2, cstring("requests").as_ptr());
        }
        AppEvent::UnbanRequestsFailed { channel, error } => {
            push_channel_table(L, channel);
            lua_setfield(L, -2, cstring("channel").as_ptr());
            set_field_string(L, -1, "error", error);
        }
        AppEvent::UnbanRequestUpsert { channel, request } => {
            push_channel_table(L, channel);
            lua_setfield(L, -2, cstring("channel").as_ptr());
            push_unban_request_table(L, request);
            lua_setfield(L, -2, cstring("request").as_ptr());
        }
        AppEvent::UnbanRequestResolved {
            channel,
            request_id,
            status,
        } => {
            push_channel_table(L, channel);
            lua_setfield(L, -2, cstring("channel").as_ptr());
            set_field_string(L, -1, "request_id", request_id);
            set_field_string(L, -1, "status", status);
        }
        AppEvent::OpenModerationTools { channel } => {
            if let Some(channel) = channel.as_ref() {
                push_channel_table(L, channel);
                lua_setfield(L, -2, cstring("channel").as_ptr());
            }
        }
        AppEvent::HighlightRulesUpdated { rules } => {
            lua_createtable(L, rules.len() as c_int, 0);
            for (idx, rule) in rules.iter().enumerate() {
                lua_createtable(L, 0, 0);
                set_field_string(L, -1, "pattern", &rule.pattern);
                set_field_bool(L, -1, "is_regex", rule.is_regex);
                set_field_bool(L, -1, "case_sensitive", rule.case_sensitive);
                set_field_bool(L, -1, "enabled", rule.enabled);
                set_field_bool(L, -1, "show_in_mentions", rule.show_in_mentions);
                if let Some(color) = rule.color.as_ref() {
                    lua_createtable(L, 3, 0);
                    lua_pushinteger(L, color[0] as lua_Integer);
                    lua_seti(L, -2, 1);
                    lua_pushinteger(L, color[1] as lua_Integer);
                    lua_seti(L, -2, 2);
                    lua_pushinteger(L, color[2] as lua_Integer);
                    lua_seti(L, -2, 3);
                    lua_setfield(L, -2, cstring("color").as_ptr());
                }
                set_field_bool(L, -1, "has_alert", rule.has_alert);
                set_field_bool(L, -1, "has_sound", rule.has_sound);
                if let Some(sound_url) = rule.sound_url.as_ref() {
                    set_field_string(L, -1, "sound_url", sound_url);
                }
                lua_seti(L, -2, (idx + 1) as lua_Integer);
            }
            lua_setfield(L, -2, cstring("rules").as_ptr());
        }
        AppEvent::FilterRecordsUpdated { records } => {
            lua_createtable(L, records.len() as c_int, 0);
            for (idx, record) in records.iter().enumerate() {
                lua_createtable(L, 0, 0);
                set_field_string(L, -1, "name", &record.name);
                set_field_string(L, -1, "pattern", &record.pattern);
                set_field_bool(L, -1, "is_regex", record.is_regex);
                set_field_bool(L, -1, "case_sensitive", record.case_sensitive);
                set_field_bool(L, -1, "enabled", record.enabled);
                match &record.scope {
                    crust_core::model::filters::FilterScope::Global => {
                        set_field_string(L, -1, "scope", "Global");
                    }
                    crust_core::model::filters::FilterScope::Channel(channel) => {
                        set_field_string(L, -1, "scope", "Channel");
                        push_channel_table(L, channel);
                        lua_setfield(L, -2, cstring("channel").as_ptr());
                    }
                }
                match &record.action {
                    crust_core::model::filters::FilterAction::Hide => {
                        set_field_string(L, -1, "action", "Hide");
                    }
                    crust_core::model::filters::FilterAction::Dim => {
                        set_field_string(L, -1, "action", "Dim");
                    }
                }
                set_field_bool(L, -1, "filter_sender", record.filter_sender);
                lua_seti(L, -2, (idx + 1) as lua_Integer);
            }
            lua_setfield(L, -2, cstring("records").as_ptr());
        }
        AppEvent::ModActionPresetsUpdated { presets } => {
            lua_createtable(L, presets.len() as c_int, 0);
            for (idx, preset) in presets.iter().enumerate() {
                lua_createtable(L, 0, 0);
                set_field_string(L, -1, "label", &preset.label);
                set_field_string(L, -1, "command_template", &preset.command_template);
                if let Some(icon_url) = preset.icon_url.as_ref() {
                    set_field_string(L, -1, "icon_url", icon_url);
                }
                lua_seti(L, -2, (idx + 1) as lua_Integer);
            }
            lua_setfield(L, -2, cstring("presets").as_ptr());
        }
        AppEvent::NicknamesUpdated { nicknames } => {
            lua_createtable(L, nicknames.len() as c_int, 0);
            for (idx, n) in nicknames.iter().enumerate() {
                lua_createtable(L, 0, 0);
                set_field_string(L, -1, "login", &n.login);
                set_field_string(L, -1, "nickname", &n.nickname);
                set_field_bool(L, -1, "replace_mentions", n.replace_mentions);
                set_field_bool(L, -1, "case_sensitive", n.case_sensitive);
                if let Some(channel) = n.channel.as_ref() {
                    set_field_string(L, -1, "channel", channel);
                }
                lua_seti(L, -2, (idx + 1) as lua_Integer);
            }
            lua_setfield(L, -2, cstring("nicknames").as_ptr());
        }
        AppEvent::IgnoredUsersUpdated { users } => {
            lua_createtable(L, users.len() as c_int, 0);
            for (idx, u) in users.iter().enumerate() {
                lua_createtable(L, 0, 0);
                set_field_string(L, -1, "login", &u.login);
                set_field_bool(L, -1, "is_regex", u.is_regex);
                set_field_bool(L, -1, "case_sensitive", u.case_sensitive);
                set_field_bool(L, -1, "enabled", u.enabled);
                lua_seti(L, -2, (idx + 1) as lua_Integer);
            }
            lua_setfield(L, -2, cstring("users").as_ptr());
        }
        AppEvent::IgnoredPhrasesUpdated { phrases } => {
            lua_createtable(L, phrases.len() as c_int, 0);
            for (idx, p) in phrases.iter().enumerate() {
                lua_createtable(L, 0, 0);
                set_field_string(L, -1, "pattern", &p.pattern);
                set_field_bool(L, -1, "is_regex", p.is_regex);
                set_field_bool(L, -1, "case_sensitive", p.case_sensitive);
                set_field_bool(L, -1, "enabled", p.enabled);
                set_field_string(L, -1, "replace_with", &p.replace_with);
                let action = match p.action {
                    crust_core::ignores::IgnoredPhraseAction::Block => "Block",
                    crust_core::ignores::IgnoredPhraseAction::Replace => "Replace",
                    crust_core::ignores::IgnoredPhraseAction::HighlightOnly => "HighlightOnly",
                    crust_core::ignores::IgnoredPhraseAction::MentionOnly => "MentionOnly",
                };
                set_field_string(L, -1, "action", action);
                lua_seti(L, -2, (idx + 1) as lua_Integer);
            }
            lua_setfield(L, -2, cstring("phrases").as_ptr());
        }
        AppEvent::UserPronounsLoaded { login, pronouns } => {
            set_field_string(L, -1, "login", login);
            if let Some(p) = pronouns.as_ref() {
                set_field_string(L, -1, "pronouns", p);
            }
        }
        AppEvent::UsercardSettingsUpdated { show_pronouns } => {
            set_field_bool(L, -1, "show_pronouns", *show_pronouns);
        }
        AppEvent::SelfAvatarLoaded { avatar_url } => {
            set_field_string(L, -1, "avatar_url", avatar_url);
        }
        AppEvent::LinkPreviewReady {
            url,
            title,
            description,
            thumbnail_url,
            site_name,
        } => {
            set_field_string(L, -1, "url", url);
            if let Some(title) = title.as_ref() {
                set_field_string(L, -1, "title", title);
            }
            if let Some(description) = description.as_ref() {
                set_field_string(L, -1, "description", description);
            }
            if let Some(thumbnail_url) = thumbnail_url.as_ref() {
                set_field_string(L, -1, "thumbnail_url", thumbnail_url);
            }
            if let Some(site_name) = site_name.as_ref() {
                set_field_string(L, -1, "site_name", site_name);
            }
        }
        AppEvent::SenderCosmeticsUpdated {
            user_id,
            color,
            name_paint,
            badge,
            avatar_url,
        } => {
            set_field_string(L, -1, "user_id", user_id);
            if let Some(color) = color.as_ref() {
                set_field_string(L, -1, "color", color);
            }
            if let Some(name_paint) = name_paint.as_ref() {
                push_sender_name_paint_table(L, name_paint);
                lua_setfield(L, -2, cstring("name_paint").as_ptr());
            }
            if let Some(badge) = badge.as_ref() {
                push_badge_table(L, badge);
                lua_setfield(L, -2, cstring("badge").as_ptr());
            }
            if let Some(avatar_url) = avatar_url.as_ref() {
                set_field_string(L, -1, "avatar_url", avatar_url);
            }
        }
        AppEvent::IrcTopicChanged { channel, topic } => {
            push_channel_table(L, channel);
            lua_setfield(L, -2, cstring("channel").as_ptr());
            set_field_string(L, -1, "topic", topic);
        }
        AppEvent::AuthExpired => {}
        AppEvent::PluginUiAction {
            plugin_name,
            surface_kind,
            surface_id,
            widget_id,
            action,
            value,
            form_values,
        } => {
            set_field_string(L, -1, "plugin_name", plugin_name);
            set_field_string(
                L,
                -1,
                "surface_kind",
                match surface_kind {
                    PluginUiSurfaceKind::Window => "window",
                    PluginUiSurfaceKind::SettingsPage => "settings_page",
                    PluginUiSurfaceKind::HostPanel => "host_panel",
                },
            );
            set_field_string(L, -1, "surface_id", surface_id);
            set_field_string(L, -1, "widget_id", widget_id);
            if let Some(action) = action.as_ref() {
                set_field_string(L, -1, "action", action);
            }
            if let Some(value) = value.as_ref() {
                push_plugin_ui_value(L, value);
                lua_setfield(L, -2, cstring("value").as_ptr());
            }
            plugin_ui_value_map_to_lua_table(L, form_values);
            lua_setfield(L, -2, cstring("form_values").as_ptr());
        }
        AppEvent::PluginUiChange {
            plugin_name,
            surface_kind,
            surface_id,
            widget_id,
            value,
            form_values,
        } => {
            set_field_string(L, -1, "plugin_name", plugin_name);
            set_field_string(
                L,
                -1,
                "surface_kind",
                match surface_kind {
                    PluginUiSurfaceKind::Window => "window",
                    PluginUiSurfaceKind::SettingsPage => "settings_page",
                    PluginUiSurfaceKind::HostPanel => "host_panel",
                },
            );
            set_field_string(L, -1, "surface_id", surface_id);
            set_field_string(L, -1, "widget_id", widget_id);
            push_plugin_ui_value(L, value);
            lua_setfield(L, -2, cstring("value").as_ptr());
            plugin_ui_value_map_to_lua_table(L, form_values);
            lua_setfield(L, -2, cstring("form_values").as_ptr());
        }
        AppEvent::PluginUiSubmit {
            plugin_name,
            surface_kind,
            surface_id,
            widget_id,
            action,
            form_values,
        } => {
            set_field_string(L, -1, "plugin_name", plugin_name);
            set_field_string(
                L,
                -1,
                "surface_kind",
                match surface_kind {
                    PluginUiSurfaceKind::Window => "window",
                    PluginUiSurfaceKind::SettingsPage => "settings_page",
                    PluginUiSurfaceKind::HostPanel => "host_panel",
                },
            );
            set_field_string(L, -1, "surface_id", surface_id);
            if let Some(widget_id) = widget_id.as_ref() {
                set_field_string(L, -1, "widget_id", widget_id);
            }
            if let Some(action) = action.as_ref() {
                set_field_string(L, -1, "action", action);
            }
            plugin_ui_value_map_to_lua_table(L, form_values);
            lua_setfield(L, -2, cstring("form_values").as_ptr());
        }
        AppEvent::PluginUiWindowClosed {
            plugin_name,
            window_id,
        } => {
            set_field_string(L, -1, "plugin_name", plugin_name);
            set_field_string(L, -1, "window_id", window_id);
        }
        AppEvent::UserStateUpdated {
            channel,
            is_mod,
            badges,
            color,
        } => {
            push_channel_table(L, channel);
            lua_setfield(L, -2, cstring("channel").as_ptr());
            set_field_bool(L, -1, "is_mod", *is_mod);
            lua_createtable(L, badges.len() as c_int, 0);
            for (idx, badge) in badges.iter().enumerate() {
                push_badge_table(L, badge);
                lua_seti(L, -2, (idx + 1) as lua_Integer);
            }
            lua_setfield(L, -2, cstring("badges").as_ptr());
            if let Some(color) = color.as_ref() {
                set_field_string(L, -1, "color", color);
            }
        }
        AppEvent::UpdateAvailable {
            version,
            release_url,
            asset_name,
        } => {
            set_field_string(L, -1, "version", version);
            set_field_string(L, -1, "release_url", release_url);
            set_field_string(L, -1, "asset_name", asset_name);
        }
        AppEvent::UpdateCheckUpToDate { version } => {
            set_field_string(L, -1, "version", version);
        }
        AppEvent::UpdateCheckFailed { message, manual } => {
            set_field_string(L, -1, "message", message);
            set_field_bool(L, -1, "manual", *manual);
        }
        AppEvent::UpdaterSettingsUpdated {
            update_checks_enabled,
            last_checked_at,
            skipped_version,
        } => {
            set_field_bool(L, -1, "update_checks_enabled", *update_checks_enabled);
            if let Some(last_checked_at) = last_checked_at.as_ref() {
                set_field_string(L, -1, "last_checked_at", last_checked_at);
            }
            set_field_string(L, -1, "skipped_version", skipped_version);
        }
        AppEvent::UpdateInstallStarted { version } => {
            set_field_string(L, -1, "version", version);
        }
        AppEvent::UpdateInstallScheduled {
            version,
            restart_now,
        } => {
            set_field_string(L, -1, "version", version);
            set_field_bool(L, -1, "restart_now", *restart_now);
        }
        AppEvent::UpdateInstallFailed { version, message } => {
            set_field_string(L, -1, "version", version);
            set_field_string(L, -1, "message", message);
        }
        AppEvent::StreamerModeSettingsUpdated {
            mode,
            hide_link_previews,
            hide_viewer_counts,
            suppress_sounds,
        } => {
            set_field_string(L, -1, "mode", mode);
            set_field_bool(L, -1, "hide_link_previews", *hide_link_previews);
            set_field_bool(L, -1, "hide_viewer_counts", *hide_viewer_counts);
            set_field_bool(L, -1, "suppress_sounds", *suppress_sounds);
        }
        AppEvent::StreamerModeActiveChanged { active } => {
            set_field_bool(L, -1, "active", *active);
        }
    }

    lua_gettop(L)
}

unsafe fn call_event_callback(
    L: *mut lua_State,
    func_ref: c_int,
    arg_index: c_int,
) -> Result<(), String> {
    lua_rawgeti(L, LUA_REGISTRYINDEX, func_ref as lua_Integer);
    lua_pushvalue(L, arg_index);
    let rc = lua_pcallk(L, 1, 0, 0, 0, None);
    if rc != LUA_OK {
        return Err(lua_error_text(L, rc));
    }
    Ok(())
}

unsafe fn configure_package_path(L: *mut lua_State, plugin_dir: &Path) {
    lua_getglobal(L, cstring("package").as_ptr());
    if lua_type(L, -1) == LUA_TTABLE {
        lua_getfield(L, -1, cstring("path").as_ptr());
        let existing = lua_value_string(L, -1).unwrap_or_default();
        lua_pop(L, 1);
        let plugin = normalize_lua_path(&plugin_dir.to_string_lossy());
        let new_path = format!("{}/?.lua;{}/?/init.lua;{}", plugin, plugin, existing);
        lua_pushstring(L, cstring(&new_path).as_ptr());
        lua_setfield(L, -2, cstring("path").as_ptr());
    }
    lua_pop(L, 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    use crust_core::plugins::{PluginHost, PluginUiHostSlot};

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    struct LuaStateGuard(*mut lua_State);

    impl LuaStateGuard {
        fn new() -> Self {
            unsafe {
                let L = luaL_newstate();
                assert!(!L.is_null(), "failed to create Lua state");
                luaL_openselectedlibs(L, -1, 0);
                Self(L)
            }
        }

        fn ptr(&self) -> *mut lua_State {
            self.0
        }
    }

    impl Drop for LuaStateGuard {
        fn drop(&mut self) {
            unsafe {
                lua_close(self.0);
            }
        }
    }

    fn lua_get_string_field(L: *mut lua_State, idx: c_int, key: &str) -> Option<String> {
        unsafe {
            let idx = lua_absindex(L, idx);
            lua_getfield(L, idx, cstring(key).as_ptr());
            let out = lua_value_string(L, -1);
            lua_pop(L, 1);
            out
        }
    }

    fn lua_get_int_field(L: *mut lua_State, idx: c_int, key: &str) -> Option<i64> {
        unsafe {
            let idx = lua_absindex(L, idx);
            lua_getfield(L, idx, cstring(key).as_ptr());
            let out = lua_value_int(L, -1);
            lua_pop(L, 1);
            out
        }
    }

    fn lua_get_table_len_field(L: *mut lua_State, idx: c_int, key: &str) -> usize {
        unsafe {
            let idx = lua_absindex(L, idx);
            lua_getfield(L, idx, cstring(key).as_ptr());
            let len = if lua_type(L, -1) == LUA_TTABLE {
                lua_rawlen(L, -1)
            } else {
                0
            };
            lua_pop(L, 1);
            len
        }
    }

    fn recv_cmd_with_timeout(
        rx: &mut tokio::sync::mpsc::Receiver<AppCommand>,
        timeout_ms: u64,
    ) -> Option<AppCommand> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            match rx.try_recv() {
                Ok(cmd) => return Some(cmd),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return None,
            }
        }
    }

    #[test]
    fn event_table_builds_authenticated_payload() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let lua = LuaStateGuard::new();
        let event = AppEvent::Authenticated {
            username: "alice".to_owned(),
            user_id: "42".to_owned(),
        };
        let idx = unsafe { make_event_table(lua.ptr(), &event) };

        assert_eq!(
            lua_get_string_field(lua.ptr(), idx, "type").as_deref(),
            Some("Authenticated")
        );
        assert_eq!(
            lua_get_string_field(lua.ptr(), idx, "username").as_deref(),
            Some("alice")
        );
        assert_eq!(
            lua_get_string_field(lua.ptr(), idx, "user_id").as_deref(),
            Some("42")
        );
    }

    #[test]
    fn callback_event_parser_rejects_unknown_values() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let lua = LuaStateGuard::new();
        unsafe {
            lua_pushinteger(lua.ptr(), 999);
            assert!(event_kind_from_value(lua.ptr(), -1).is_none());
            lua_pop(lua.ptr(), 1);

            lua_pushstring(lua.ptr(), cstring("not_a_real_event").as_ptr());
            assert!(event_kind_from_value(lua.ptr(), -1).is_none());
        }
    }

    #[test]
    fn callback_event_parser_accepts_new_values() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let lua = LuaStateGuard::new();
        unsafe {
            lua_pushstring(lua.ptr(), cstring("EmoteImageReady").as_ptr());
            assert_eq!(
                event_kind_from_value(lua.ptr(), -1),
                Some(PluginEventKind::EmoteImageReady)
            );
            lua_pop(lua.ptr(), 1);

            lua_pushstring(lua.ptr(), cstring("EmoteCatalogUpdated").as_ptr());
            assert_eq!(
                event_kind_from_value(lua.ptr(), -1),
                Some(PluginEventKind::EmoteCatalogUpdated)
            );
            lua_pop(lua.ptr(), 1);

            lua_pushstring(lua.ptr(), cstring("UserMessagesCleared").as_ptr());
            assert_eq!(
                event_kind_from_value(lua.ptr(), -1),
                Some(PluginEventKind::UserMessagesCleared)
            );
            lua_pop(lua.ptr(), 1);

            lua_pushstring(lua.ptr(), cstring("ImagePrefetchQueued").as_ptr());
            assert_eq!(
                event_kind_from_value(lua.ptr(), -1),
                Some(PluginEventKind::ImagePrefetchQueued)
            );
            lua_pop(lua.ptr(), 1);
        }
    }

    #[test]
    fn event_table_builds_emote_image_ready_payload() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let lua = LuaStateGuard::new();
        let event = AppEvent::EmoteImageReady {
            uri: "emote://test".to_owned(),
            width: 32,
            height: 18,
            raw_bytes: vec![1, 2, 3],
        };
        let idx = unsafe { make_event_table(lua.ptr(), &event) };

        assert_eq!(
            lua_get_string_field(lua.ptr(), idx, "type").as_deref(),
            Some("EmoteImageReady")
        );
        assert_eq!(
            lua_get_string_field(lua.ptr(), idx, "uri").as_deref(),
            Some("emote://test")
        );
        assert_eq!(lua_get_int_field(lua.ptr(), idx, "width"), Some(32));
        assert_eq!(lua_get_int_field(lua.ptr(), idx, "height"), Some(18));
        assert_eq!(
            lua_get_string_field(lua.ptr(), idx, "raw_bytes_base64").as_deref(),
            Some("AQID")
        );
        assert_eq!(lua_get_string_field(lua.ptr(), idx, "raw_bytes"), None);
    }

    #[test]
    fn event_table_builds_emote_catalog_payload() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let lua = LuaStateGuard::new();
        let event = AppEvent::EmoteCatalogUpdated {
            emotes: vec![
                EmoteCatalogEntry {
                    code: "Kappa".to_owned(),
                    provider: "twitch".to_owned(),
                    url: "https://example.invalid/kappa.png".to_owned(),
                    scope: "global".to_owned(),
                },
                EmoteCatalogEntry {
                    code: "Pog".to_owned(),
                    provider: "7tv".to_owned(),
                    url: "https://example.invalid/pog.png".to_owned(),
                    scope: "channel".to_owned(),
                },
            ],
        };
        let idx = unsafe { make_event_table(lua.ptr(), &event) };

        assert_eq!(
            lua_get_string_field(lua.ptr(), idx, "type").as_deref(),
            Some("EmoteCatalogUpdated")
        );
        assert_eq!(lua_get_table_len_field(lua.ptr(), idx, "emotes"), 2);
    }

    #[test]
    fn event_table_builds_user_messages_cleared_payload() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let lua = LuaStateGuard::new();
        let event = AppEvent::UserMessagesCleared {
            channel: ChannelId::new("some_channel"),
            login: "alice".to_owned(),
        };
        let idx = unsafe { make_event_table(lua.ptr(), &event) };

        assert_eq!(
            lua_get_string_field(lua.ptr(), idx, "type").as_deref(),
            Some("UserMessagesCleared")
        );
        assert_eq!(
            lua_get_string_field(lua.ptr(), idx, "login").as_deref(),
            Some("alice")
        );
    }

    #[test]
    fn event_table_builds_image_prefetch_queued_payload() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let lua = LuaStateGuard::new();
        let event = AppEvent::ImagePrefetchQueued { count: 7 };
        let idx = unsafe { make_event_table(lua.ptr(), &event) };

        assert_eq!(
            lua_get_string_field(lua.ptr(), idx, "type").as_deref(),
            Some("ImagePrefetchQueued")
        );
        assert_eq!(lua_get_int_field(lua.ptr(), idx, "count"), Some(7));
    }

    #[test]
    fn event_table_builds_plugin_ui_change_payload() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let lua = LuaStateGuard::new();
        let mut form_values = BTreeMap::new();
        form_values.insert("name".to_owned(), PluginUiValue::String("Crust".to_owned()));
        let event = AppEvent::PluginUiChange {
            plugin_name: "demo".to_owned(),
            surface_kind: PluginUiSurfaceKind::Window,
            surface_id: "main".to_owned(),
            widget_id: "name".to_owned(),
            value: PluginUiValue::String("Crust".to_owned()),
            form_values,
        };
        let idx = unsafe { make_event_table(lua.ptr(), &event) };

        assert_eq!(
            lua_get_string_field(lua.ptr(), idx, "type").as_deref(),
            Some("PluginUiChange")
        );
        assert_eq!(
            lua_get_string_field(lua.ptr(), idx, "surface_kind").as_deref(),
            Some("window")
        );
        assert_eq!(
            lua_get_string_field(lua.ptr(), idx, "widget_id").as_deref(),
            Some("name")
        );
    }

    #[test]
    fn event_table_builds_host_panel_payload() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let lua = LuaStateGuard::new();
        let mut form_values = BTreeMap::new();
        form_values.insert(
            "mode".to_owned(),
            PluginUiValue::String("compact".to_owned()),
        );
        let event = AppEvent::PluginUiSubmit {
            plugin_name: "demo".to_owned(),
            surface_kind: PluginUiSurfaceKind::HostPanel,
            surface_id: "appearance_tools".to_owned(),
            widget_id: Some("apply".to_owned()),
            action: Some("apply".to_owned()),
            form_values,
        };
        let idx = unsafe { make_event_table(lua.ptr(), &event) };

        assert_eq!(
            lua_get_string_field(lua.ptr(), idx, "type").as_deref(),
            Some("PluginUiSubmit")
        );
        assert_eq!(
            lua_get_string_field(lua.ptr(), idx, "surface_kind").as_deref(),
            Some("host_panel")
        );
        assert_eq!(
            lua_get_string_field(lua.ptr(), idx, "surface_id").as_deref(),
            Some("appearance_tools")
        );
        assert_eq!(
            lua_get_string_field(lua.ptr(), idx, "widget_id").as_deref(),
            Some("apply")
        );
    }

    #[test]
    fn callback_is_removed_when_plugin_unloads() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root =
            std::env::temp_dir().join(format!("crust-lua-api-test-{}", system_time_unix_ms()));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("event_probe");
        fs::create_dir_all(&plugin_dir).unwrap();

        let info = r#"{
  "name": "event_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#;
        fs::write(plugin_dir.join("info.json"), info).unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.register_callback(c2.EventType.Authenticated, function(ev)
  c2.add_system_message("system", ev.username or "missing")
end)
"#,
        )
        .unwrap();

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let statuses = host.plugin_statuses();
        assert!(
            statuses.iter().any(|status| status.loaded),
            "test plugin should load: {statuses:#?}"
        );

        let event = AppEvent::Authenticated {
            username: "alice".to_owned(),
            user_id: "42".to_owned(),
        };
        host.dispatch_event(&event);

        let first =
            recv_cmd_with_timeout(&mut cmd_rx, 2_000).expect("callback should send a command");
        match first {
            AppCommand::InjectLocalMessage { channel, text } => {
                assert_eq!(channel, ChannelId::new("system"));
                assert_eq!(text, "alice");
            }
            other => panic!("unexpected command from callback: {other:?}"),
        }

        fs::remove_dir_all(&plugin_dir).unwrap();
        host.reload();
        host.dispatch_event(&event);
        assert!(cmd_rx.try_recv().is_err());

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn ui_window_registration_is_available_to_ui_snapshot() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root =
            std::env::temp_dir().join(format!("crust-plugin-ui-window-{}", system_time_unix_ms()));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("ui_window_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "ui_window_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.ui.register_window({
  id = "main",
  title = "Demo Window",
  open = true,
  children = {
    { type = "heading", text = "Plugin Window" },
    { type = "button", id = "save", text = "Save", action = "save" }
  }
})
"#,
        )
        .unwrap();

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let snapshot = host.plugin_ui_snapshot();
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].plugin_name, "ui_window_probe");
        assert_eq!(snapshot.windows[0].window.id, "main");
        assert_eq!(snapshot.windows[0].window.title, "Demo Window");
        assert_eq!(snapshot.windows[0].window.children.len(), 2);

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn ui_settings_page_registration_is_available_to_ui_snapshot() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-plugin-ui-settings-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("ui_settings_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "ui_settings_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.ui.register_settings_page({
  id = "settings",
  title = "Settings Demo",
  summary = "Plugin-controlled settings",
  children = {
    { type = "checkbox", id = "enabled", text = "Enabled", value = true }
  }
})
"#,
        )
        .unwrap();

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let snapshot = host.plugin_ui_snapshot();
        assert_eq!(snapshot.settings_pages.len(), 1);
        assert_eq!(snapshot.settings_pages[0].plugin_name, "ui_settings_probe");
        assert_eq!(snapshot.settings_pages[0].page.id, "settings");
        assert_eq!(snapshot.settings_pages[0].page.title, "Settings Demo");

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn host_panel_registration_is_available_to_ui_snapshot() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-plugin-ui-host-panel-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("ui_host_panel_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "ui_host_panel_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.ui.register_host_panel({
  id = "appearance_tools",
  slot = "settings.appearance",
  title = "Appearance Tools",
  summary = "Plugin-owned controls",
  order = 25,
  children = {
    { type = "text", text = "Hello host panel" },
    { type = "button", id = "refresh", text = "Refresh", action = "refresh" }
  }
})
"#,
        )
        .unwrap();

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let snapshot = host.plugin_ui_snapshot();
        assert_eq!(snapshot.host_panels.len(), 1);
        assert_eq!(snapshot.host_panels[0].plugin_name, "ui_host_panel_probe");
        assert_eq!(snapshot.host_panels[0].panel.id, "appearance_tools");
        assert_eq!(
            snapshot.host_panels[0].panel.slot,
            PluginUiHostSlot::SettingsAppearance
        );
        assert_eq!(
            snapshot.host_panels[0].panel.slot.as_str(),
            "settings.appearance"
        );
        assert_eq!(
            snapshot.host_panels[0].panel.title.as_deref(),
            Some("Appearance Tools")
        );
        assert_eq!(
            snapshot.host_panels[0].panel.summary.as_deref(),
            Some("Plugin-owned controls")
        );
        assert_eq!(snapshot.host_panels[0].panel.order, 25);
        assert_eq!(snapshot.host_panels[0].panel.children.len(), 2);

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn invalid_ui_window_spec_is_ignored() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-plugin-ui-invalid-window-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("ui_invalid_window_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "ui_invalid_window_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.ui.register_window({
  title = "Missing ID"
})
"#,
        )
        .unwrap();

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let snapshot = host.plugin_ui_snapshot();
        assert!(snapshot.windows.is_empty());

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn invalid_host_panel_spec_is_ignored() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-plugin-ui-invalid-host-panel-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("ui_invalid_host_panel_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "ui_invalid_host_panel_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.ui.register_host_panel({
  id = "broken_panel",
  title = "Missing slot"
})

c2.ui.register_host_panel({
  id = "bad_slot_panel",
  slot = "settings.unknown",
  title = "Bad slot"
})
"#,
        )
        .unwrap();

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let snapshot = host.plugin_ui_snapshot();
        assert!(snapshot.host_panels.is_empty());

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn invalid_ui_settings_page_spec_is_ignored() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-plugin-ui-invalid-settings-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("ui_invalid_settings_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "ui_invalid_settings_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.ui.register_settings_page("not_a_table")
"#,
        )
        .unwrap();

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let snapshot = host.plugin_ui_snapshot();
        assert!(snapshot.settings_pages.is_empty());

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn ui_window_commands_update_snapshot_state() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-plugin-ui-window-commands-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("ui_window_command_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "ui_window_command_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.ui.register_window({
  id = "main",
  open = false,
  children = {
    { type = "text", text = "Window" }
  }
})

c2.register_command("openui", function(ctx)
  c2.ui.open_window("main")
end)

c2.register_command("closeui", function(ctx)
  c2.ui.close_window("main")
end)

c2.register_command("removeui", function(ctx)
  c2.ui.unregister_window("main")
end)
"#,
        )
        .unwrap();

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let mut snapshot = host.plugin_ui_snapshot();
        assert_eq!(snapshot.windows.len(), 1);
        assert!(!snapshot.windows[0].window.open);

        host.execute_command(PluginCommandInvocation {
            command: "openui".into(),
            channel: ChannelId::new("system"),
            words: vec![],
            reply_to_msg_id: None,
            reply: None,
            raw_text: "/openui".into(),
        });
        snapshot = host.plugin_ui_snapshot();
        assert!(snapshot.windows[0].window.open);

        host.execute_command(PluginCommandInvocation {
            command: "closeui".into(),
            channel: ChannelId::new("system"),
            words: vec![],
            reply_to_msg_id: None,
            reply: None,
            raw_text: "/closeui".into(),
        });
        snapshot = host.plugin_ui_snapshot();
        assert!(!snapshot.windows[0].window.open);

        host.execute_command(PluginCommandInvocation {
            command: "removeui".into(),
            channel: ChannelId::new("system"),
            words: vec![],
            reply_to_msg_id: None,
            reply: None,
            raw_text: "/removeui".into(),
        });
        snapshot = host.plugin_ui_snapshot();
        assert!(snapshot.windows.is_empty());

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn host_panel_update_uses_explicit_id() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-plugin-ui-host-panel-update-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("ui_host_panel_update_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "ui_host_panel_update_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.ui.register_host_panel({
  id = "alpha",
  slot = "settings.appearance",
  title = "Alpha",
  children = {
    { type = "text", text = "before" }
  }
})

c2.ui.update_host_panel("beta", {
  id = "ignored",
  slot = "settings.chat",
  title = "Beta",
  children = {
    { type = "text", text = "after" }
  }
})
"#,
        )
        .unwrap();

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let snapshot = host.plugin_ui_snapshot();
        assert_eq!(snapshot.host_panels.len(), 2);
        assert!(snapshot
            .host_panels
            .iter()
            .any(|panel| panel.panel.id == "alpha"));
        let updated = snapshot
            .host_panels
            .iter()
            .find(|panel| panel.panel.id == "beta")
            .expect("updated host panel should use explicit id");
        assert_eq!(updated.panel.title.as_deref(), Some("Beta"));
        assert_eq!(updated.panel.slot, PluginUiHostSlot::SettingsChat);

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn host_panel_commands_update_snapshot_state() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-plugin-ui-host-panel-commands-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("ui_host_panel_command_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "ui_host_panel_command_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.ui.register_host_panel({
  id = "main",
  slot = "settings.appearance",
  title = "Before",
  children = {
    { type = "text", text = "before" }
  }
})

c2.register_command("updatehostpanel", function(ctx)
  c2.ui.update_host_panel("main", {
    id = "ignored",
    slot = "settings.chat",
    title = "After",
    children = {
      { type = "text", text = "after" }
    }
  })
end)

c2.register_command("removehostpanel", function(ctx)
  c2.ui.unregister_host_panel("main")
end)
"#,
        )
        .unwrap();

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let mut snapshot = host.plugin_ui_snapshot();
        assert_eq!(snapshot.host_panels.len(), 1);
        assert_eq!(
            snapshot.host_panels[0].panel.title.as_deref(),
            Some("Before")
        );
        assert_eq!(
            snapshot.host_panels[0].panel.slot,
            PluginUiHostSlot::SettingsAppearance
        );

        host.execute_command(PluginCommandInvocation {
            command: "updatehostpanel".into(),
            channel: ChannelId::new("system"),
            words: vec![],
            reply_to_msg_id: None,
            reply: None,
            raw_text: "/updatehostpanel".into(),
        });
        snapshot = host.plugin_ui_snapshot();
        assert_eq!(snapshot.host_panels.len(), 1);
        assert_eq!(snapshot.host_panels[0].panel.id, "main");
        assert_eq!(
            snapshot.host_panels[0].panel.title.as_deref(),
            Some("After")
        );
        assert_eq!(
            snapshot.host_panels[0].panel.slot,
            PluginUiHostSlot::SettingsChat
        );

        host.execute_command(PluginCommandInvocation {
            command: "removehostpanel".into(),
            channel: ChannelId::new("system"),
            words: vec![],
            reply_to_msg_id: None,
            reply: None,
            raw_text: "/removehostpanel".into(),
        });
        snapshot = host.plugin_ui_snapshot();
        assert!(snapshot.host_panels.is_empty());

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn ui_host_panels_demo_plugin_loads() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-plugin-ui-host-panel-example-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let source_plugin = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/ui_host_panels_demo_plugin");
        let plugin_dir = plugin_root.join("ui_host_panels_demo_plugin");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::copy(
            source_plugin.join("info.json"),
            plugin_dir.join("info.json"),
        )
        .unwrap();
        fs::copy(source_plugin.join("init.lua"), plugin_dir.join("init.lua")).unwrap();

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let statuses = host.plugin_statuses();
        assert!(
            statuses.iter().any(|status| status.loaded),
            "host panel example plugin should load: {statuses:#?}"
        );

        let snapshot = host.plugin_ui_snapshot();
        let plugin_panels: Vec<_> = snapshot
            .host_panels
            .iter()
            .filter(|panel| panel.plugin_name == "UI Host Panels Demo")
            .collect();
        assert_eq!(plugin_panels.len(), 3);
        assert!(snapshot.host_panels.iter().any(|panel| {
            panel.plugin_name == "UI Host Panels Demo"
                && panel.panel.id == "appearance_tools"
                && panel.panel.slot == PluginUiHostSlot::SettingsAppearance
        }));
        assert!(snapshot.host_panels.iter().any(|panel| {
            panel.plugin_name == "UI Host Panels Demo"
                && panel.panel.id == "sidebar_quick_actions"
                && panel.panel.slot == PluginUiHostSlot::SidebarTop
        }));
        assert!(snapshot.host_panels.iter().any(|panel| {
            panel.plugin_name == "UI Host Panels Demo"
                && panel.panel.id == "active_channel_tools"
                && panel.panel.slot == PluginUiHostSlot::ChannelHeader
        }));

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn clock_usage_plugin_loads_and_registers_command() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root =
            std::env::temp_dir().join(format!("crust-clock-usage-smoke-{}", system_time_unix_ms()));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let source_plugin =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/clock_usage_plugin");
        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("clock_usage_plugin");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::copy(
            source_plugin.join("info.json"),
            plugin_dir.join("info.json"),
        )
        .unwrap();
        fs::copy(source_plugin.join("init.lua"), plugin_dir.join("init.lua")).unwrap();

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let statuses = host.plugin_statuses();
        assert!(
            statuses.iter().any(|status| status.loaded),
            "clock_usage_plugin should load: {statuses:#?}"
        );

        let commands = host.command_infos();
        assert!(commands.iter().any(|cmd| cmd.name == "crusttime"));

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn current_channel_binding_returns_nil_without_active_channel() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-current-channel-nil-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("current_channel_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "current_channel_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.register_command("showcurrentchannel", function(ctx)
  local channel = c2.current_channel()
  c2.add_system_message(ctx.channel, channel and (channel.display_name or channel.name or "missing") or "(none)")
end)
"#,
        )
        .unwrap();

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        host.execute_command(PluginCommandInvocation {
            command: "showcurrentchannel".into(),
            channel: ChannelId::new("system"),
            words: vec!["showcurrentchannel".into()],
            reply_to_msg_id: None,
            reply: None,
            raw_text: "/showcurrentchannel".into(),
        });

        let first =
            recv_cmd_with_timeout(&mut cmd_rx, 2_000).expect("command should emit a local message");
        match first {
            AppCommand::InjectLocalMessage { channel, text } => {
                assert_eq!(channel, ChannelId::new("system"));
                assert_eq!(text, "(none)");
            }
            other => panic!("unexpected command from current_channel probe: {other:?}"),
        }

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn current_channel_binding_returns_active_channel_snapshot() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-current-channel-active-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("current_channel_active_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "current_channel_active_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.register_command("showactivechannel", function(ctx)
  local channel = c2.current_channel()
  c2.add_system_message(ctx.channel, channel and (channel.display_name or channel.name or "missing") or "(none)")
end)
"#,
        )
        .unwrap();

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        host.set_current_channel(Some(ChannelId::new("zackrawrr")));

        host.execute_command(PluginCommandInvocation {
            command: "showactivechannel".into(),
            channel: ChannelId::new("system"),
            words: vec!["showactivechannel".into()],
            reply_to_msg_id: None,
            reply: None,
            raw_text: "/showactivechannel".into(),
        });

        let first =
            recv_cmd_with_timeout(&mut cmd_rx, 2_000).expect("command should emit a local message");
        match first {
            AppCommand::InjectLocalMessage { channel, text } => {
                assert_eq!(channel, ChannelId::new("system"));
                assert_eq!(text, "zackrawrr");
            }
            other => panic!("unexpected command from current_channel probe: {other:?}"),
        }

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn api_tour_plugin_loads() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root =
            std::env::temp_dir().join(format!("crust-api-tour-smoke-{}", system_time_unix_ms()));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let source_plugin =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/api_tour_plugin");
        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("api_tour_plugin");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::copy(
            source_plugin.join("info.json"),
            plugin_dir.join("info.json"),
        )
        .unwrap();
        fs::copy(source_plugin.join("init.lua"), plugin_dir.join("init.lua")).unwrap();

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let statuses = host.plugin_statuses();
        assert!(
            statuses.iter().any(|status| status.loaded),
            "api_tour_plugin should load: {statuses:#?}"
        );

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn event_callback_demo_plugin_loads() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-event-callback-demo-smoke-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let source_plugin = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/event_callback_demo_plugin");
        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("event_callback_demo_plugin");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::copy(
            source_plugin.join("info.json"),
            plugin_dir.join("info.json"),
        )
        .unwrap();
        fs::copy(source_plugin.join("init.lua"), plugin_dir.join("init.lua")).unwrap();

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let statuses = host.plugin_statuses();
        assert!(
            statuses.iter().any(|status| status.loaded),
            "event_callback_demo_plugin should load: {statuses:#?}"
        );

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn loader_progress_demo_plugin_loads() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-loader-progress-demo-smoke-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let source_plugin = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/loader_progress_demo_plugin");
        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("loader_progress_demo_plugin");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::copy(
            source_plugin.join("info.json"),
            plugin_dir.join("info.json"),
        )
        .unwrap();
        fs::copy(source_plugin.join("init.lua"), plugin_dir.join("init.lua")).unwrap();

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let statuses = host.plugin_statuses();
        assert!(
            statuses.iter().any(|status| status.loaded),
            "loader_progress_demo_plugin should load: {statuses:#?}"
        );

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn clock_usage_plugin_delayed_tick_callback_runs() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root =
            std::env::temp_dir().join(format!("crust-clock-usage-delay-{}", system_time_unix_ms()));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let source_plugin =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/clock_usage_plugin");
        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("clock_usage_plugin");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::copy(
            source_plugin.join("info.json"),
            plugin_dir.join("info.json"),
        )
        .unwrap();
        fs::copy(source_plugin.join("init.lua"), plugin_dir.join("init.lua")).unwrap();

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(32);
        let host = LuaPluginHost::new(cmd_tx, true);

        host.execute_command(PluginCommandInvocation {
            command: "crusttime".into(),
            channel: ChannelId::new("system"),
            words: vec!["crusttime".into()],
            reply_to_msg_id: None,
            reply: None,
            raw_text: "/crusttime".into(),
        });

        let snapshot = host.plugin_ui_snapshot();
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].window.id, "clock_usage");

        let delayed =
            recv_cmd_with_timeout(&mut cmd_rx, 2_500).expect("clock plugin should schedule a tick");
        let (vm_key, callback_ref) = match delayed {
            AppCommand::InjectLocalMessage { .. } => {
                let next = recv_cmd_with_timeout(&mut cmd_rx, 2_500)
                    .expect("clock plugin should schedule a tick after opening");
                match next {
                    AppCommand::RunPluginCallback {
                        vm_key,
                        callback_ref,
                    } => (vm_key, callback_ref),
                    other => panic!("unexpected follow-up command from clock plugin: {other:?}"),
                }
            }
            AppCommand::RunPluginCallback {
                vm_key,
                callback_ref,
            } => (vm_key, callback_ref),
            other => panic!("unexpected first command from clock plugin: {other:?}"),
        };

        host.run_plugin_callback(vm_key, callback_ref);

        let snapshot = host.plugin_ui_snapshot();
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].window.id, "clock_usage");

        let follow_up = recv_cmd_with_timeout(&mut cmd_rx, 2_500)
            .expect("clock tick callback should re-arm itself");
        match follow_up {
            AppCommand::RunPluginCallback { .. } => {}
            other => panic!("unexpected follow-up command from clock plugin: {other:?}"),
        }

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn ui_window_showcase_plugin_loads() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-ui-window-showcase-smoke-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let source_plugin = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/ui_window_showcase_plugin");
        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("ui_window_showcase_plugin");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::copy(
            source_plugin.join("info.json"),
            plugin_dir.join("info.json"),
        )
        .unwrap();
        fs::copy(source_plugin.join("init.lua"), plugin_dir.join("init.lua")).unwrap();

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let statuses = host.plugin_statuses();
        assert!(
            statuses.iter().any(|status| status.loaded),
            "ui_window_showcase_plugin should load: {statuses:#?}"
        );

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn ui_settings_demo_plugin_loads() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-ui-settings-demo-smoke-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let source_plugin =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/ui_settings_demo_plugin");
        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("ui_settings_demo_plugin");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::copy(
            source_plugin.join("info.json"),
            plugin_dir.join("info.json"),
        )
        .unwrap();
        fs::copy(source_plugin.join("init.lua"), plugin_dir.join("init.lua")).unwrap();

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let statuses = host.plugin_statuses();
        assert!(
            statuses.iter().any(|status| status.loaded),
            "ui_settings_demo_plugin should load: {statuses:#?}"
        );

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn send_message_binding_supports_reply_to_msg_id() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-send-message-reply-id-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("send_message_reply_id_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "send_message_reply_id_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.send_message("some_channel", "hello", {
  reply_to_msg_id = "parent-123"
})
"#,
        )
        .unwrap();

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let cmd = recv_cmd_with_timeout(&mut cmd_rx, 2_000)
            .expect("send_message reply_to_msg_id binding should send a command");
        match cmd {
            AppCommand::SendMessage {
                channel,
                text,
                reply_to_msg_id,
                reply,
            } => {
                assert_eq!(channel, ChannelId::new("some_channel"));
                assert_eq!(text, "hello");
                assert_eq!(reply_to_msg_id.as_deref(), Some("parent-123"));
                assert!(reply.is_none());
            }
            other => panic!("unexpected command from send_message binding: {other:?}"),
        }
        assert!(host.plugin_statuses().iter().any(|status| status.loaded));

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn send_message_binding_supports_full_reply_metadata() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-send-message-reply-full-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("send_message_reply_full_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "send_message_reply_full_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.send_message("some_channel", "hello", {
  reply_to_msg_id = "parent-123",
  reply = {
    parent_msg_id = "parent-123",
    parent_user_login = "alice",
    parent_display_name = "Alice",
    parent_msg_body = "original message"
  }
})
"#,
        )
        .unwrap();

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let cmd = recv_cmd_with_timeout(&mut cmd_rx, 2_000)
            .expect("send_message full reply binding should send a command");
        match cmd {
            AppCommand::SendMessage {
                channel,
                text,
                reply_to_msg_id,
                reply,
            } => {
                assert_eq!(channel, ChannelId::new("some_channel"));
                assert_eq!(text, "hello");
                assert_eq!(reply_to_msg_id.as_deref(), Some("parent-123"));
                let reply = reply.expect("reply metadata should be present");
                assert_eq!(reply.parent_msg_id, "parent-123");
                assert_eq!(reply.parent_user_login, "alice");
                assert_eq!(reply.parent_display_name, "Alice");
                assert_eq!(reply.parent_msg_body, "original message");
            }
            other => panic!("unexpected command from send_message binding: {other:?}"),
        }
        assert!(host.plugin_statuses().iter().any(|status| status.loaded));

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn send_message_binding_ignores_malformed_reply_options() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-send-message-reply-invalid-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("send_message_reply_invalid_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "send_message_reply_invalid_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.send_message("some_channel", "hello", {
  reply_to_msg_id = 123,
  reply = {
    parent_msg_id = "parent-123",
    parent_user_login = false
  }
})
"#,
        )
        .unwrap();

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let cmd = recv_cmd_with_timeout(&mut cmd_rx, 2_000)
            .expect("send_message malformed reply options should still send a command");
        match cmd {
            AppCommand::SendMessage {
                channel,
                text,
                reply_to_msg_id,
                reply,
            } => {
                assert_eq!(channel, ChannelId::new("some_channel"));
                assert_eq!(text, "hello");
                assert!(reply_to_msg_id.is_none());
                assert!(reply.is_none());
            }
            other => panic!("unexpected command from send_message binding: {other:?}"),
        }
        assert!(host.plugin_statuses().iter().any(|status| status.loaded));

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn emote_image_ready_callback_dispatches_payload() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-emote-image-callback-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("emote_image_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "emote_image_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.register_callback(c2.EventType.EmoteImageReady, function(ev)
  c2.add_system_message("system", string.format("%s:%s", tostring(ev.width), tostring(ev.raw_bytes_base64)))
end)
"#,
        )
        .unwrap();

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let event = AppEvent::EmoteImageReady {
            uri: "emote://probe".to_owned(),
            width: 32,
            height: 32,
            raw_bytes: vec![1, 2, 3],
        };
        host.dispatch_event(&event);

        let first = recv_cmd_with_timeout(&mut cmd_rx, 2_000)
            .expect("emote image callback should send a command");
        match first {
            AppCommand::InjectLocalMessage { channel, text } => {
                assert_eq!(channel, ChannelId::new("system"));
                assert_eq!(text, "32:AQID");
            }
            other => panic!("unexpected command from callback: {other:?}"),
        }

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn image_prefetch_queued_callback_dispatches_payload() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-image-prefetch-callback-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("image_prefetch_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "image_prefetch_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.register_callback(c2.EventType.ImagePrefetchQueued, function(ev)
  c2.add_system_message("system", "queued:" .. tostring(ev.count))
end)
"#,
        )
        .unwrap();

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let event = AppEvent::ImagePrefetchQueued { count: 5 };
        host.dispatch_event(&event);

        let first = recv_cmd_with_timeout(&mut cmd_rx, 2_000)
            .expect("image prefetch callback should send a command");
        match first {
            AppCommand::InjectLocalMessage { channel, text } => {
                assert_eq!(channel, ChannelId::new("system"));
                assert_eq!(text, "queued:5");
            }
            other => panic!("unexpected command from callback: {other:?}"),
        }

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn plugin_ui_action_callback_dispatches_payload() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-plugin-ui-action-callback-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("plugin_ui_action_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "plugin_ui_action_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.register_callback(c2.EventType.PluginUiAction, function(ev)
  c2.add_system_message("system", (ev.widget_id or "missing") .. ":" .. (ev.action or ""))
end)
"#,
        )
        .unwrap();

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        host.dispatch_event(&AppEvent::PluginUiAction {
            plugin_name: "plugin_ui_action_probe".to_owned(),
            surface_kind: PluginUiSurfaceKind::Window,
            surface_id: "main".to_owned(),
            widget_id: "save".to_owned(),
            action: Some("save".to_owned()),
            value: None,
            form_values: BTreeMap::new(),
        });

        let first = recv_cmd_with_timeout(&mut cmd_rx, 2_000)
            .expect("plugin ui action callback should send a command");
        match first {
            AppCommand::InjectLocalMessage { channel, text } => {
                assert_eq!(channel, ChannelId::new("system"));
                assert_eq!(text, "save:save");
            }
            other => panic!("unexpected command from callback: {other:?}"),
        }

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn plugin_ui_submit_callback_dispatches_form_values() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-plugin-ui-submit-callback-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("plugin_ui_submit_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "plugin_ui_submit_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.register_callback(c2.EventType.PluginUiSubmit, function(ev)
  c2.add_system_message("system", tostring(ev.form_values.name or "missing"))
end)
"#,
        )
        .unwrap();

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let mut form_values = BTreeMap::new();
        form_values.insert("name".to_owned(), PluginUiValue::String("Crust".to_owned()));
        host.dispatch_event(&AppEvent::PluginUiSubmit {
            plugin_name: "plugin_ui_submit_probe".to_owned(),
            surface_kind: PluginUiSurfaceKind::SettingsPage,
            surface_id: "settings".to_owned(),
            widget_id: Some("submit".to_owned()),
            action: Some("save".to_owned()),
            form_values,
        });

        let first = recv_cmd_with_timeout(&mut cmd_rx, 2_000)
            .expect("plugin ui submit callback should send a command");
        match first {
            AppCommand::InjectLocalMessage { channel, text } => {
                assert_eq!(channel, ChannelId::new("system"));
                assert_eq!(text, "Crust");
            }
            other => panic!("unexpected command from callback: {other:?}"),
        }

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn plugin_ui_event_dispatch_is_scoped_to_target_plugin() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-plugin-ui-target-scope-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        for plugin_name in ["plugin_ui_scope_alpha", "plugin_ui_scope_beta"] {
            let plugin_dir = plugin_root.join(plugin_name);
            fs::create_dir_all(&plugin_dir).unwrap();
            fs::write(
                plugin_dir.join("info.json"),
                format!(
                    r#"{{
  "name": "{plugin_name}",
  "version": "0.1.0",
  "entry": "init.lua"
}}"#
                ),
            )
            .unwrap();
            fs::write(
                plugin_dir.join("init.lua"),
                format!(
                    r#"
c2.register_callback(c2.EventType.PluginUiAction, function(ev)
  c2.add_system_message("system", "{plugin_name}:" .. tostring(ev.widget_id or "missing"))
end)
"#
                ),
            )
            .unwrap();
        }

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        host.dispatch_event(&AppEvent::PluginUiAction {
            plugin_name: "plugin_ui_scope_beta".to_owned(),
            surface_kind: PluginUiSurfaceKind::Window,
            surface_id: "main".to_owned(),
            widget_id: "save".to_owned(),
            action: Some("save".to_owned()),
            value: None,
            form_values: BTreeMap::new(),
        });

        let first = recv_cmd_with_timeout(&mut cmd_rx, 2_000)
            .expect("only the targeted plugin should receive the UI event");
        match first {
            AppCommand::InjectLocalMessage { channel, text } => {
                assert_eq!(channel, ChannelId::new("system"));
                assert_eq!(text, "plugin_ui_scope_beta:save");
            }
            other => panic!("unexpected command from scoped callback: {other:?}"),
        }
        assert!(cmd_rx.try_recv().is_err());

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn ui_callback_is_removed_when_plugin_unloads() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-plugin-ui-unload-callback-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("plugin_ui_unload_probe");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "plugin_ui_unload_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.register_callback(c2.EventType.PluginUiAction, function(ev)
  c2.add_system_message("system", "ui-unload")
end)
"#,
        )
        .unwrap();

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        host.dispatch_event(&AppEvent::PluginUiAction {
            plugin_name: "plugin_ui_unload_probe".to_owned(),
            surface_kind: PluginUiSurfaceKind::Window,
            surface_id: "main".to_owned(),
            widget_id: "save".to_owned(),
            action: Some("save".to_owned()),
            value: None,
            form_values: BTreeMap::new(),
        });

        let first =
            recv_cmd_with_timeout(&mut cmd_rx, 2_000).expect("callback should send a command");
        match first {
            AppCommand::InjectLocalMessage { channel, text } => {
                assert_eq!(channel, ChannelId::new("system"));
                assert_eq!(text, "ui-unload");
            }
            other => panic!("unexpected command from callback: {other:?}"),
        }

        fs::remove_dir_all(&plugin_dir).unwrap();
        host.reload();
        host.dispatch_event(&AppEvent::PluginUiAction {
            plugin_name: "plugin_ui_unload_probe".to_owned(),
            surface_kind: PluginUiSurfaceKind::Window,
            surface_id: "main".to_owned(),
            widget_id: "save".to_owned(),
            action: Some("save".to_owned()),
            value: None,
            form_values: BTreeMap::new(),
        });
        assert!(cmd_rx.try_recv().is_err());

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn reward_redemption_status_binding_sends_command() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root =
            std::env::temp_dir().join(format!("crust-reward-binding-{}", system_time_unix_ms()));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("reward_probe");
        fs::create_dir_all(&plugin_dir).unwrap();

        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "reward_probe",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.update_reward_redemption_status(
  "some_channel",
  "reward-123",
  "redemption-456",
  "FULFILLED",
  "alice",
  "VIP Song Request"
)
"#,
        )
        .unwrap();

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        let cmd = recv_cmd_with_timeout(&mut cmd_rx, 2_000)
            .expect("reward redemption binding should send a command");
        match cmd {
            AppCommand::UpdateRewardRedemptionStatus {
                channel,
                reward_id,
                redemption_id,
                status,
                user_login,
                reward_title,
            } => {
                assert_eq!(channel, ChannelId::new("some_channel"));
                assert_eq!(reward_id, "reward-123");
                assert_eq!(redemption_id, "redemption-456");
                assert_eq!(status, "FULFILLED");
                assert_eq!(user_login, "alice");
                assert_eq!(reward_title, "VIP Song Request");
            }
            other => panic!("unexpected command from reward binding: {other:?}"),
        }
        assert!(host.plugin_statuses().iter().any(|status| status.loaded));

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn reward_redemption_status_binding_ignores_invalid_args() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp_root = std::env::temp_dir().join(format!(
            "crust-reward-binding-invalid-{}",
            system_time_unix_ms()
        ));
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &temp_root);
        std::env::set_var("XDG_DATA_HOME", temp_root.join(".local/share"));

        let plugin_root = LuaPluginHost::plugin_root_dir();
        let plugin_dir = plugin_root.join("reward_probe_invalid");
        fs::create_dir_all(&plugin_dir).unwrap();

        fs::write(
            plugin_dir.join("info.json"),
            r#"{
  "name": "reward_probe_invalid",
  "version": "0.1.0",
  "entry": "init.lua"
}"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("init.lua"),
            r#"
c2.update_reward_redemption_status("some_channel")
"#,
        )
        .unwrap();

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<AppCommand>(8);
        let host = LuaPluginHost::new(cmd_tx, true);
        assert!(host.plugin_statuses().iter().any(|status| status.loaded));
        assert!(cmd_rx.try_recv().is_err());

        if let Some(value) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(value) = old_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        let _ = fs::remove_dir_all(&temp_root);
    }
}

pub fn init_plugins(
    cmd_tx: mpsc::Sender<AppCommand>,
    use_24h_timestamps: bool,
) -> Arc<LuaPluginHost> {
    LuaPluginHost::new(cmd_tx, use_24h_timestamps)
}

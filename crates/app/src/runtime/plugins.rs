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

use chrono::Utc;
use directories::ProjectDirs;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crust_core::events::AppCommand;
use crust_core::model::{ChannelId, Platform};
use crust_core::plugins::{
    PluginAuthSnapshot, PluginChannelSnapshot, PluginCommandInfo, PluginCommandInvocation,
    PluginCompletionList, PluginCompletionRequest, PluginHost, PluginManifestInfo, PluginStatus,
};

use super::system_messages::make_system_message;

static PLUGIN_HOST: OnceLock<Arc<LuaPluginHost>> = OnceLock::new();
static PLUGIN_STATE_INDEX: OnceLock<RwLock<HashMap<usize, usize>>> = OnceLock::new();

fn set_global_host(host: Arc<LuaPluginHost>) {
    let _ = PLUGIN_HOST.set(host);
}

fn global_host() -> Option<Arc<LuaPluginHost>> {
    PLUGIN_HOST.get().map(Arc::clone)
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
    completion_callbacks: Vec<c_int>,
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
            for callback in self.completion_callbacks.drain(..) {
                if callback != LUA_NOREF && callback != LUA_REFNIL {
                    luaL_unref(self.vm, LUA_REGISTRYINDEX, callback);
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
            let mut guard = plugin_state_index().write().unwrap_or_else(|p| p.into_inner());
            guard.clear();
        }
        {
            let mut guard = self.statuses.write().unwrap_or_else(|p| p.into_inner());
            guard.clear();
        }
        {
            let mut guard = self.command_index.write().unwrap_or_else(|p| p.into_inner());
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
        let manifest: PluginManifest = serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)?;
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
            completion_callbacks: Vec::new(),
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
            let mut guard = plugin_state_index().write().unwrap_or_else(|p| p.into_inner());
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
            let mut index = self.command_index.write().unwrap_or_else(|p| p.into_inner());
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

            lua_createtable(vm, 0, 0);
            set_field_int(vm, -1, "CompletionRequested", 0);
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
            register_c2_fn(vm, c2_index, "send_message", native_send_message, plugin_idx);
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
            lua_setglobal(vm, cstring("c2").as_ptr());

            lua_pushinteger(vm, plugin_idx as lua_Integer);
            lua_pushcclosure(vm, Some(native_print), 1);
            lua_setglobal(vm, cstring("print").as_ptr());

            configure_package_path(vm, &runtime.lock().unwrap_or_else(|p| p.into_inner()).root_dir);
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
        self.auth
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn session_started_unix_ms(&self) -> i64 {
        self.session_started_unix_ms
    }
}

impl PluginHost for LuaPluginHost {
    fn plugin_statuses(&self) -> Vec<PluginStatus> {
        self.statuses
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
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
                (guard.vm, guard.completion_callbacks.clone(), guard.manifest.name.clone())
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
                        warn!("plugins: completion callback in {} failed: {err}", plugin_name);
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

    let mut index = host.command_index.write().unwrap_or_else(|p| p.into_inner());
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
    let event_name = lua_value_string(L, 1).unwrap_or_default();
    let event_id = lua_value_int(L, 1);
    if event_id != Some(0) && !event_name.ends_with("CompletionRequested") {
        return 0;
    }
    let Some(runtime) = host.runtime_by_index(plugin_idx) else {
        return 0;
    };
    let mut guard = runtime.lock().unwrap_or_else(|p| p.into_inner());
    lua_pushvalue(L, 2);
    let callback_ref = luaL_ref(L, LUA_REGISTRYINDEX);
    guard.completion_callbacks.push(callback_ref);
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
    host.send_command(AppCommand::SendMessage {
        channel,
        text,
        reply_to_msg_id: None,
        reply: None,
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

fn set_list_string(L: *mut lua_State, idx: c_int, pos: lua_Integer, value: &str) {
    unsafe {
        let idx = lua_absindex(L, idx);
        lua_pushstring(L, cstring(value).as_ptr());
        lua_seti(L, idx, pos);
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
                let out = lua_value_string(L, -1)
                    .and_then(|name| ChannelId::parse_user_input(&name).or_else(|| Some(ChannelId::new(name))));
                lua_pop(L, 1);
                if out.is_some() {
                    return out;
                }
            }
        }
        None
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

unsafe fn configure_package_path(L: *mut lua_State, plugin_dir: &Path) {
    lua_getglobal(L, cstring("package").as_ptr());
    if lua_type(L, -1) == LUA_TTABLE {
        lua_getfield(L, -1, cstring("path").as_ptr());
        let existing = lua_value_string(L, -1).unwrap_or_default();
        lua_pop(L, 1);
        let plugin = normalize_lua_path(&plugin_dir.to_string_lossy());
        let new_path = format!(
            "{}/?.lua;{}/?/init.lua;{}",
            plugin,
            plugin,
            existing
        );
        lua_pushstring(L, cstring(&new_path).as_ptr());
        lua_setfield(L, -2, cstring("path").as_ptr());
    }
    lua_pop(L, 1);
}

pub fn init_plugins(cmd_tx: mpsc::Sender<AppCommand>, use_24h_timestamps: bool) -> Arc<LuaPluginHost> {
    LuaPluginHost::new(cmd_tx, use_24h_timestamps)
}

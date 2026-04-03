use std::sync::{Arc, OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::model::{ChannelId, ReplyInfo};

/// Metadata for a slash command registered by a plugin.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginCommandInfo {
    pub name: String,
    pub usage: String,
    pub summary: String,
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// Manifest metadata for a plugin, shown in the UI and logs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginManifestInfo {
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
    #[serde(default)]
    pub entry: String,
}

/// Status summary for one plugin directory.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginStatus {
    pub manifest: PluginManifestInfo,
    #[serde(default)]
    pub loaded: bool,
    #[serde(default)]
    pub command_count: usize,
    #[serde(default)]
    pub error: Option<String>,
}

/// Completion request forwarded to plugin callbacks.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginCompletionRequest {
    pub query: String,
    pub full_text_content: String,
    pub cursor_position: usize,
    pub is_first_word: bool,
    #[serde(default)]
    pub channel: Option<ChannelId>,
}

/// Completion result returned by plugin callbacks.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginCompletionList {
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(default)]
    pub hide_others: bool,
}

/// Auth snapshot exposed to plugins.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginAuthSnapshot {
    #[serde(default)]
    pub logged_in: bool,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

/// Per-channel moderation snapshot exposed to plugins.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginChannelSnapshot {
    #[serde(default)]
    pub is_joined: bool,
    #[serde(default)]
    pub is_mod: bool,
    #[serde(default)]
    pub is_vip: bool,
    #[serde(default)]
    pub is_broadcaster: bool,
}

/// Invocation payload for a registered plugin command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCommandInvocation {
    pub command: String,
    pub channel: ChannelId,
    pub words: Vec<String>,
    #[serde(default)]
    pub reply_to_msg_id: Option<String>,
    #[serde(default)]
    pub reply: Option<ReplyInfo>,
    #[serde(default)]
    pub raw_text: String,
}

/// Runtime bridge implemented by the app crate.
pub trait PluginHost: Send + Sync {
    fn plugin_statuses(&self) -> Vec<PluginStatus>;
    fn command_infos(&self) -> Vec<PluginCommandInfo>;
    fn complete_command(&self, request: PluginCompletionRequest) -> PluginCompletionList;
    fn execute_command(&self, invocation: PluginCommandInvocation);
    fn run_plugin_callback(&self, vm_key: usize, callback_ref: i32);
    fn use_24h_timestamps(&self) -> bool;
    fn set_use_24h_timestamps(&self, enabled: bool);
    fn reload(&self);
    fn update_auth_snapshot(&self, snapshot: PluginAuthSnapshot);
    fn update_channel_snapshot(&self, channel: ChannelId, snapshot: PluginChannelSnapshot);
}

static PLUGIN_HOST: OnceLock<RwLock<Option<Arc<dyn PluginHost>>>> = OnceLock::new();

fn plugin_host_slot() -> &'static RwLock<Option<Arc<dyn PluginHost>>> {
    PLUGIN_HOST.get_or_init(|| RwLock::new(None))
}

/// Install the global plugin host bridge.
pub fn set_plugin_host(host: Arc<dyn PluginHost>) -> bool {
    let slot = plugin_host_slot();
    let mut guard = slot.write().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_some() {
        return false;
    }
    *guard = Some(host);
    true
}

/// Get the current plugin host bridge if one is installed.
pub fn plugin_host() -> Option<Arc<dyn PluginHost>> {
    let slot = plugin_host_slot();
    let guard = slot.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.as_ref().map(Arc::clone)
}

/// Return the dynamic slash-command metadata registered by plugins.
pub fn plugin_command_infos() -> Vec<PluginCommandInfo> {
    plugin_host()
        .map(|host| host.command_infos())
        .unwrap_or_default()
}

/// Ask plugins to contribute completions for a slash-command request.
pub fn plugin_command_completion(request: PluginCompletionRequest) -> PluginCompletionList {
    plugin_host()
        .map(|host| host.complete_command(request))
        .unwrap_or_default()
}

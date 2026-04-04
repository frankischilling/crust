use std::sync::{Arc, OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::events::AppEvent;
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

/// Which retained plugin UI surface emitted an interaction.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PluginUiSurfaceKind {
    Window,
    SettingsPage,
    HostPanel,
}

/// Named host-owned insertion points that plugin panels can target.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PluginUiHostSlot {
    SettingsIntegrations,
    SettingsAppearance,
    SettingsChat,
    SidebarTop,
    ChannelHeader,
}

impl PluginUiHostSlot {
    pub fn as_str(self) -> &'static str {
        match self {
            PluginUiHostSlot::SettingsIntegrations => "settings.integrations",
            PluginUiHostSlot::SettingsAppearance => "settings.appearance",
            PluginUiHostSlot::SettingsChat => "settings.chat",
            PluginUiHostSlot::SidebarTop => "sidebar.top",
            PluginUiHostSlot::ChannelHeader => "channel_header",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "settings.integrations" => Some(PluginUiHostSlot::SettingsIntegrations),
            "settings.appearance" => Some(PluginUiHostSlot::SettingsAppearance),
            "settings.chat" => Some(PluginUiHostSlot::SettingsChat),
            "sidebar.top" => Some(PluginUiHostSlot::SidebarTop),
            "channel_header" => Some(PluginUiHostSlot::ChannelHeader),
            _ => None,
        }
    }
}

/// A typed value exchanged by plugin UI widgets and callbacks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "value")]
pub enum PluginUiValue {
    String(String),
    Bool(bool),
    Number(f64),
    Strings(Vec<String>),
}

/// Shared style hints accepted by plugin UI surfaces and widgets.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PluginUiStyle {
    #[serde(default)]
    pub visible: Option<bool>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub width: Option<f32>,
    #[serde(default)]
    pub height: Option<f32>,
    #[serde(default)]
    pub min_width: Option<f32>,
    #[serde(default)]
    pub min_height: Option<f32>,
    #[serde(default)]
    pub max_width: Option<f32>,
    #[serde(default)]
    pub max_height: Option<f32>,
    #[serde(default)]
    pub padding: Option<f32>,
    #[serde(default)]
    pub align: Option<String>,
    #[serde(default)]
    pub text_role: Option<String>,
    #[serde(default)]
    pub emphasis: Option<String>,
    #[serde(default)]
    pub border_color: Option<String>,
    #[serde(default)]
    pub fill_color: Option<String>,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
}

/// One choice entry for `radio_group` or `select`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginUiChoice {
    pub label: String,
    pub value: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// One row item for the `list` widget.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginUiListItem {
    pub label: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// One column definition for the `table` widget.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginUiTableColumn {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub align: Option<String>,
}

/// A retained declarative widget node.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PluginUiWidget {
    pub kind: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub value: Option<PluginUiValue>,
    #[serde(default)]
    pub progress: Option<f32>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default)]
    pub rows: Vec<Vec<PluginUiValue>>,
    #[serde(default)]
    pub children: Vec<PluginUiWidget>,
    #[serde(default)]
    pub options: Vec<PluginUiChoice>,
    #[serde(default)]
    pub items: Vec<PluginUiListItem>,
    #[serde(default)]
    pub columns: Vec<PluginUiTableColumn>,
    #[serde(default)]
    pub form_key: Option<String>,
    #[serde(default)]
    pub host_form: bool,
    #[serde(default)]
    pub submit: bool,
    #[serde(default)]
    pub open: Option<bool>,
    #[serde(default)]
    pub style: PluginUiStyle,
}

/// Retained floating window owned by one plugin.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PluginUiWindowSpec {
    pub id: String,
    pub title: String,
    #[serde(default = "default_true")]
    pub open: bool,
    #[serde(default = "default_true")]
    pub resizable: bool,
    #[serde(default)]
    pub scroll: bool,
    #[serde(default)]
    pub default_width: Option<f32>,
    #[serde(default)]
    pub default_height: Option<f32>,
    #[serde(default)]
    pub min_width: Option<f32>,
    #[serde(default)]
    pub min_height: Option<f32>,
    #[serde(default)]
    pub max_width: Option<f32>,
    #[serde(default)]
    pub max_height: Option<f32>,
    #[serde(default)]
    pub children: Vec<PluginUiWidget>,
    #[serde(default)]
    pub style: PluginUiStyle,
}

/// Retained settings page contributed by one plugin into the shared Plugins hub.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PluginUiSettingsPageSpec {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub children: Vec<PluginUiWidget>,
    #[serde(default)]
    pub style: PluginUiStyle,
}

/// Retained host-panel contributed by one plugin into a named host slot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginUiHostPanelSpec {
    pub id: String,
    pub slot: PluginUiHostSlot,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub order: i32,
    #[serde(default)]
    pub children: Vec<PluginUiWidget>,
    #[serde(default)]
    pub style: PluginUiStyle,
}

impl Default for PluginUiHostPanelSpec {
    fn default() -> Self {
        Self {
            id: String::new(),
            slot: PluginUiHostSlot::SettingsIntegrations,
            title: None,
            summary: None,
            order: 0,
            children: Vec::new(),
            style: PluginUiStyle::default(),
        }
    }
}

/// One plugin-owned window registration visible to the UI crate.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PluginUiWindowRegistration {
    pub plugin_name: String,
    pub window: PluginUiWindowSpec,
}

/// One plugin-owned settings-page registration visible to the UI crate.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PluginUiSettingsPageRegistration {
    pub plugin_name: String,
    pub page: PluginUiSettingsPageSpec,
}

/// One plugin-owned host-panel registration visible to the UI crate.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PluginUiHostPanelRegistration {
    pub plugin_name: String,
    pub panel: PluginUiHostPanelSpec,
}

/// Snapshot of all retained plugin UI surfaces currently registered.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PluginUiSnapshot {
    #[serde(default)]
    pub windows: Vec<PluginUiWindowRegistration>,
    #[serde(default)]
    pub settings_pages: Vec<PluginUiSettingsPageRegistration>,
    #[serde(default)]
    pub host_panels: Vec<PluginUiHostPanelRegistration>,
}

fn default_true() -> bool {
    true
}

/// Runtime bridge implemented by the app crate.
pub trait PluginHost: Send + Sync {
    fn plugin_statuses(&self) -> Vec<PluginStatus>;
    fn plugin_ui_snapshot(&self) -> PluginUiSnapshot;
    fn command_infos(&self) -> Vec<PluginCommandInfo>;
    fn complete_command(&self, request: PluginCompletionRequest) -> PluginCompletionList;
    fn execute_command(&self, invocation: PluginCommandInvocation);
    fn dispatch_event(&self, event: &AppEvent);
    fn run_plugin_callback(&self, vm_key: usize, callback_ref: i32);
    fn use_24h_timestamps(&self) -> bool;
    fn set_use_24h_timestamps(&self, enabled: bool);
    fn reload(&self);
    fn update_auth_snapshot(&self, snapshot: PluginAuthSnapshot);
    fn update_channel_snapshot(&self, channel: ChannelId, snapshot: PluginChannelSnapshot);
    fn set_current_channel(&self, channel: Option<ChannelId>);
    fn set_plugin_window_open(&self, plugin_name: &str, window_id: &str, open: bool);
}

static PLUGIN_HOST: OnceLock<RwLock<Option<Arc<dyn PluginHost>>>> = OnceLock::new();

fn plugin_host_slot() -> &'static RwLock<Option<Arc<dyn PluginHost>>> {
    PLUGIN_HOST.get_or_init(|| RwLock::new(None))
}

/// Install the global plugin host bridge.
pub fn set_plugin_host(host: Arc<dyn PluginHost>) -> bool {
    let slot = plugin_host_slot();
    let mut guard = slot
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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

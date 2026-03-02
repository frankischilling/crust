use std::collections::HashMap;

use crate::{
    events::ConnectionState,
    model::{ChannelId, ChannelState},
};

/// Authentication state.
#[derive(Debug, Clone, Default)]
pub struct AuthState {
    /// Whether the user is logged in with an OAuth token.
    pub logged_in: bool,
    /// The authenticated username (display name).
    pub username: Option<String>,
    /// The Twitch user-id.
    pub user_id: Option<String>,
    /// CDN URL for the user's avatar image.
    pub avatar_url: Option<String>,
}

/// The single source of truth for the whole application.
#[derive(Debug)]
pub struct AppState {
    pub connection: ConnectionState,
    pub auth: AuthState,
    pub channels: HashMap<ChannelId, ChannelState>,
    /// The currently-visible channel tab.
    pub active_channel: Option<ChannelId>,
    /// Ordered list so tabs render in a stable order.
    pub channel_order: Vec<ChannelId>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            connection: ConnectionState::Disconnected,
            auth: AuthState::default(),
            channels: HashMap::new(),
            active_channel: None,
            channel_order: Vec::new(),
        }
    }
}

impl AppState {
    pub fn join_channel(&mut self, id: ChannelId) {
        if !self.channels.contains_key(&id) {
            self.channels.insert(id.clone(), ChannelState::new(id.clone()));
            self.channel_order.push(id.clone());
        }
        if self.active_channel.is_none() {
            self.active_channel = Some(id);
        }
    }

    pub fn leave_channel(&mut self, id: &ChannelId) {
        self.channels.remove(id);
        self.channel_order.retain(|c| c != id);
        if self.active_channel.as_ref() == Some(id) {
            self.active_channel = self.channel_order.first().cloned();
        }
    }

    pub fn active_state(&self) -> Option<&ChannelState> {
        self.active_channel.as_ref().and_then(|id| self.channels.get(id))
    }

    pub fn active_state_mut(&mut self) -> Option<&mut ChannelState> {
        self.active_channel
            .as_ref()
            .and_then(|id| self.channels.get_mut(id))
    }
}

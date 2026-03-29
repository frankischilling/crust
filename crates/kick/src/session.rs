use std::collections::HashMap;
use std::time::Duration;

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crust_core::model::{
    Badge, ChannelId, ChatMessage, MessageFlags, MessageId, MsgKind, Sender, SystemNotice, UserId,
};

use crate::api;
use crate::KickError;

const PUSHER_URL: &str = "wss://ws-us2.pusher.com/app/32cbd69e4b950bf97679?protocol=7&client=js&version=8.4.0-rc2&flash=false";
const BACKOFF_SECS: &[u64] = &[1, 2, 5, 10, 30];

/// Events produced by the Kick session (consumed by the app reducer).
#[derive(Debug, Clone)]
pub enum KickEvent {
    Connected,
    Disconnected,
    Reconnecting {
        attempt: u32,
    },
    ChatMessage(ChatMessage),
    MessageDeleted {
        channel: ChannelId,
        server_id: String,
    },
    ChannelInfoResolved {
        channel: ChannelId,
        chatroom_id: u64,
        user_id: u64,
    },
    SystemNotice(SystemNotice),
    Error(String),
    UserBanned {
        channel: ChannelId,
        login: String,
    },
    ChatCleared {
        channel: ChannelId,
    },
}

/// Commands consumed by the Kick session.
#[derive(Debug)]
pub enum KickSessionCommand {
    JoinChannel(ChannelId),
    LeaveChannel(ChannelId),
    Disconnect,
}

/// Pusher protocol message envelope.
#[derive(Deserialize, Debug)]
struct PusherMessage {
    event: String,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    data: Option<serde_json::Value>,
}

/// Kick chat message payload (inside Pusher data).
#[derive(Deserialize, Debug)]
struct KickChatPayload {
    id: Option<String>,
    #[allow(dead_code)]
    chatroom_id: Option<u64>,
    content: Option<String>,
    #[allow(dead_code)]
    #[serde(rename = "type")]
    msg_type: Option<String>,
    created_at: Option<String>,
    sender: Option<KickSenderPayload>,
}

#[derive(Deserialize, Debug)]
struct KickSenderPayload {
    id: Option<u64>,
    username: Option<String>,
    slug: Option<String>,
    identity: Option<KickIdentity>,
}

#[derive(Deserialize, Debug)]
struct KickIdentity {
    color: Option<String>,
    badges: Option<Vec<serde_json::Value>>,
}

/// Kick message deletion payload.
#[derive(Deserialize, Debug)]
struct KickDeletePayload {
    id: Option<String>,
    message: Option<KickDeletedMessage>,
}

#[derive(Deserialize, Debug)]
struct KickDeletedMessage {
    id: Option<String>,
}

/// Kick user banned payload.
#[derive(Deserialize, Debug)]
struct KickBannedPayload {
    user: Option<KickBannedUser>,
}

#[derive(Deserialize, Debug)]
struct KickBannedUser {
    username: Option<String>,
    slug: Option<String>,
}

#[derive(Deserialize, Debug)]
struct KickPinnedBy {
    id: Option<u64>,
    username: Option<String>,
    slug: Option<String>,
}

#[derive(Deserialize, Debug)]
struct KickPinnedMessageRef {
    id: Option<String>,
    text: Option<String>,
    content: Option<String>,
}

#[derive(Deserialize, Debug)]
struct KickPinnedCreatedPayload {
    #[serde(rename = "pinnedBy")]
    pinned_by: Option<KickPinnedBy>,
    message: Option<KickPinnedMessageRef>,
    created_at: Option<String>,
}

#[derive(Deserialize, Debug)]
struct KickPinnedDeletedPayload {
    id: Option<String>,
    message_id: Option<String>,
    message: Option<KickPinnedMessageRef>,
}

pub struct KickSession {
    event_tx: mpsc::Sender<KickEvent>,
    cmd_rx: mpsc::Receiver<KickSessionCommand>,
    next_msg_id: u64,
    /// chatroom_id → ChannelId mapping (for routing incoming Pusher events).
    chatroom_to_channel: HashMap<u64, ChannelId>,
    /// ChannelId → chatroom_id mapping (for Pusher subscriptions).
    channel_to_chatroom: HashMap<ChannelId, u64>,
    /// ChannelId → (badge key -> badge URL) fallback map, populated from
    /// Kick channel API payloads.
    channel_badge_urls: HashMap<ChannelId, HashMap<String, String>>,
    /// Channels waiting for chatroom_id resolution.
    pending_joins: Vec<ChannelId>,
}

impl KickSession {
    pub fn new(
        event_tx: mpsc::Sender<KickEvent>,
        cmd_rx: mpsc::Receiver<KickSessionCommand>,
    ) -> Self {
        Self {
            event_tx,
            cmd_rx,
            next_msg_id: 1,
            chatroom_to_channel: HashMap::new(),
            channel_to_chatroom: HashMap::new(),
            channel_badge_urls: HashMap::new(),
            pending_joins: Vec::new(),
        }
    }

    fn alloc_id(&mut self) -> MessageId {
        let id = self.next_msg_id;
        self.next_msg_id += 1;
        MessageId(id)
    }

    async fn emit(&self, event: KickEvent) {
        if self.event_tx.send(event).await.is_err() {
            warn!("Kick event channel closed; dropping event");
        }
    }

    /// Main entry point: runs the connect/reconnect loop.
    pub async fn run(mut self) {
        let mut attempt: u32 = 0;
        loop {
            if attempt > 0 {
                self.emit(KickEvent::Reconnecting { attempt }).await;
            }

            match self.connect_once().await {
                Ok(should_reconnect) => {
                    if !should_reconnect {
                        info!("Kick session disconnected cleanly");
                        self.emit(KickEvent::Disconnected).await;
                        return;
                    }
                    warn!("Kick session ended unexpectedly, will reconnect");
                }
                Err(e) => {
                    error!("Kick session error: {e}");
                    self.emit(KickEvent::Error(e.to_string())).await;
                }
            }

            let delay = BACKOFF_SECS
                .get(attempt as usize)
                .copied()
                .unwrap_or(*BACKOFF_SECS.last().unwrap());
            warn!("Kick: reconnecting in {delay}s…");
            tokio::time::sleep(Duration::from_secs(delay)).await;
            attempt += 1;
        }
    }

    async fn connect_once(&mut self) -> Result<bool, KickError> {
        info!("Connecting to Kick Pusher: {PUSHER_URL}");
        let (ws_stream, _) = connect_async(PUSHER_URL).await?;
        let (mut sink, mut stream) = ws_stream.split();

        // Process any pending joins that were queued before connection
        let pending = std::mem::take(&mut self.pending_joins);
        for ch in pending {
            self.resolve_and_subscribe(&ch, &mut sink).await;
        }

        // Re-subscribe to already-resolved channels after a reconnect
        let existing: Vec<(ChannelId, u64)> = self
            .channel_to_chatroom
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        for (_ch, chatroom_id) in &existing {
            let sub_msg = pusher_subscribe(&format!("chatrooms.{chatroom_id}.v2"));
            let _ = sink.send(Message::Text(sub_msg.into())).await;
        }

        let ping_interval = Duration::from_secs(60);
        let mut ping_timer = tokio::time::interval(ping_interval);
        ping_timer.tick().await; // first tick is immediate

        loop {
            tokio::select! {
                maybe_frame = stream.next() => {
                    match maybe_frame {
                        None => {
                            warn!("Kick WebSocket stream closed");
                            return Ok(true);
                        }
                        Some(Err(e)) => {
                            warn!("Kick WebSocket read error: {e}");
                            return Err(KickError::WebSocket(e));
                        }
                        Some(Ok(Message::Text(txt))) => {
                            self.handle_pusher_message(&txt).await;
                        }
                        Some(Ok(Message::Close(_))) => {
                            info!("Kick server close frame");
                            return Ok(true);
                        }
                        Some(Ok(_)) => {}
                    }
                }

                maybe_cmd = self.cmd_rx.recv() => {
                    match maybe_cmd {
                        None => {
                            info!("Kick command channel closed");
                            return Ok(false);
                        }
                        Some(KickSessionCommand::Disconnect) => {
                            let _ = sink.close().await;
                            return Ok(false);
                        }
                        Some(KickSessionCommand::JoinChannel(ch)) => {
                            self.resolve_and_subscribe(&ch, &mut sink).await;
                        }
                        Some(KickSessionCommand::LeaveChannel(ch)) => {
                            if let Some(chatroom_id) = self.channel_to_chatroom.remove(&ch) {
                                self.chatroom_to_channel.remove(&chatroom_id);
                                let unsub = pusher_unsubscribe(&format!("chatrooms.{chatroom_id}.v2"));
                                let _ = sink.send(Message::Text(unsub.into())).await;
                            }
                        }
                    }
                }

                _ = ping_timer.tick() => {
                    let ping = serde_json::json!({"event": "pusher:ping", "data": {}});
                    let _ = sink.send(Message::Text(ping.to_string().into())).await;
                }
            }
        }
    }

    /// Resolve a Kick channel slug to a chatroom_id and subscribe via Pusher.
    async fn resolve_and_subscribe<S>(&mut self, ch: &ChannelId, sink: &mut S)
    where
        S: futures_util::Sink<Message> + Unpin,
        <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
    {
        let slug = match ch.kick_slug() {
            Some(s) => s.to_owned(),
            None => {
                warn!("Not a Kick channel: {ch}");
                return;
            }
        };

        // Check if already resolved
        if self.channel_to_chatroom.contains_key(ch) {
            let chatroom_id = self.channel_to_chatroom[ch];
            let sub_msg = pusher_subscribe(&format!("chatrooms.{chatroom_id}.v2"));
            let _ = sink.send(Message::Text(sub_msg.into())).await;
            return;
        }

        match api::fetch_channel_info(&slug).await {
            Ok(info) => {
                let chatroom_id = info.chatroom_id;
                info!("Kick channel '{slug}' → chatroom_id={chatroom_id}");
                self.chatroom_to_channel.insert(chatroom_id, ch.clone());
                self.channel_to_chatroom.insert(ch.clone(), chatroom_id);
                self.channel_badge_urls
                    .insert(ch.clone(), info.badge_urls.clone());

                self.emit(KickEvent::ChannelInfoResolved {
                    channel: ch.clone(),
                    chatroom_id,
                    user_id: info.user_id,
                })
                .await;

                let sub_msg = pusher_subscribe(&format!("chatrooms.{chatroom_id}.v2"));
                let _ = sink.send(Message::Text(sub_msg.into())).await;
            }
            Err(e) => {
                warn!("Failed to resolve Kick channel '{slug}': {e}");
                self.emit(KickEvent::Error(format!(
                    "Could not find Kick channel '{slug}': {e}"
                )))
                .await;
            }
        }
    }

    /// Handle a raw Pusher protocol message.
    async fn handle_pusher_message(&mut self, raw: &str) {
        let msg: PusherMessage = match serde_json::from_str(raw) {
            Ok(m) => m,
            Err(e) => {
                debug!("Ignoring non-JSON Pusher frame: {e}");
                return;
            }
        };

        match msg.event.as_str() {
            "pusher:connection_established" => {
                info!("Kick Pusher connection established");
                self.emit(KickEvent::Connected).await;
            }
            "pusher:pong" => {
                debug!("Kick Pusher pong");
            }
            "pusher_internal:subscription_succeeded" => {
                if let Some(ch) = &msg.channel {
                    debug!("Subscribed to Kick channel: {ch}");
                }
            }
            "App\\Events\\ChatMessageEvent" | "App\\Events\\ChatMessageSentEvent" => {
                self.handle_chat_message(&msg).await;
            }
            "App\\Events\\MessageDeletedEvent" => {
                self.handle_message_deleted(&msg).await;
            }
            "App\\Events\\UserBannedEvent" => {
                self.handle_user_banned(&msg).await;
            }
            "App\\Events\\PinnedMessageCreatedEvent" | "PinnedMessageCreatedEvent" => {
                self.handle_pinned_message_created(&msg).await;
            }
            "App\\Events\\PinnedMessageDeletedEvent" | "PinnedMessageDeletedEvent" => {
                self.handle_pinned_message_deleted(&msg).await;
            }
            "App\\Events\\ChatroomClearEvent" => {
                self.handle_chat_cleared(&msg).await;
            }
            _ => {
                debug!("Unhandled Kick Pusher event: {}", msg.event);
            }
        }
    }

    fn channel_from_pusher(&self, pusher_channel: Option<&str>) -> Option<ChannelId> {
        let ch = pusher_channel?;
        // Format: "chatrooms.{chatroom_id}.v2"
        let parts: Vec<&str> = ch.split('.').collect();
        if parts.len() >= 2 && parts[0] == "chatrooms" {
            let chatroom_id: u64 = parts[1].parse().ok()?;
            self.chatroom_to_channel.get(&chatroom_id).cloned()
        } else {
            None
        }
    }

    /// Parse the `data` field which may be a JSON string or a JSON object.
    fn parse_data<T: serde::de::DeserializeOwned>(data: &Option<serde_json::Value>) -> Option<T> {
        let val = data.as_ref()?;
        match val {
            serde_json::Value::String(s) => serde_json::from_str(s).ok(),
            other => serde_json::from_value(other.clone()).ok(),
        }
    }

    async fn handle_chat_message(&mut self, msg: &PusherMessage) {
        let Some(channel) = self.channel_from_pusher(msg.channel.as_deref()) else {
            return;
        };

        let Some(payload) = Self::parse_data::<KickChatPayload>(&msg.data) else {
            warn!("Failed to parse Kick chat message payload");
            return;
        };

        let sender_data = payload.sender.unwrap_or(KickSenderPayload {
            id: None,
            username: None,
            slug: None,
            identity: None,
        });

        let identity = sender_data.identity.unwrap_or(KickIdentity {
            color: None,
            badges: None,
        });

        let badges: Vec<Badge> = identity
            .badges
            .unwrap_or_default()
            .into_iter()
            .filter_map(|b| {
                let name = kick_badge_name(&b)?;
                let version = kick_badge_version(&b);
                let url = kick_badge_url(&b).or_else(|| {
                    self.channel_badge_urls
                        .get(&channel)
                        .and_then(|m| resolve_kick_badge_fallback_url(m, &name, &version))
                });
                Some(Badge { name, version, url })
            })
            .collect();

        let login = sender_data
            .slug
            .or_else(|| sender_data.username.clone())
            .unwrap_or_else(|| "unknown".to_owned());

        let display_name = sender_data.username.unwrap_or_else(|| login.clone());

        let user_id_str = sender_data.id.map(|id| id.to_string()).unwrap_or_default();

        let server_id = payload.id.clone();
        let content = payload.content.unwrap_or_default();

        let timestamp = payload
            .created_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(Utc::now);

        let chat_msg = ChatMessage {
            id: self.alloc_id(),
            server_id,
            timestamp,
            channel: channel.clone(),
            sender: Sender {
                user_id: UserId(user_id_str),
                login,
                display_name,
                color: identity.color,
                name_paint: None,
                badges,
            },
            raw_text: content.clone(),
            spans: smallvec::SmallVec::new(),
            twitch_emotes: Vec::new(),
            flags: MessageFlags {
                is_action: false,
                is_highlighted: false,
                is_deleted: false,
                is_first_msg: false,
                is_pinned: false,
                is_self: false,
                is_mention: false,
                custom_reward_id: None,
                is_history: false,
            },
            reply: None,
            msg_kind: MsgKind::Chat,
        };

        self.emit(KickEvent::ChatMessage(chat_msg)).await;
    }

    async fn handle_message_deleted(&mut self, msg: &PusherMessage) {
        let Some(channel) = self.channel_from_pusher(msg.channel.as_deref()) else {
            return;
        };

        let Some(payload) = Self::parse_data::<KickDeletePayload>(&msg.data) else {
            return;
        };

        let server_id = payload
            .message
            .and_then(|m| m.id)
            .or(payload.id)
            .unwrap_or_default();

        if !server_id.is_empty() {
            self.emit(KickEvent::MessageDeleted { channel, server_id })
                .await;
        }
    }

    async fn handle_user_banned(&mut self, msg: &PusherMessage) {
        let Some(channel) = self.channel_from_pusher(msg.channel.as_deref()) else {
            return;
        };

        let Some(payload) = Self::parse_data::<KickBannedPayload>(&msg.data) else {
            return;
        };

        if let Some(user) = payload.user {
            let login = user.slug.or(user.username).unwrap_or_default();
            if !login.is_empty() {
                self.emit(KickEvent::UserBanned { channel, login }).await;
            }
        }
    }

    async fn handle_chat_cleared(&mut self, msg: &PusherMessage) {
        let Some(channel) = self.channel_from_pusher(msg.channel.as_deref()) else {
            return;
        };
        self.emit(KickEvent::ChatCleared { channel }).await;
    }

    async fn handle_pinned_message_created(&mut self, msg: &PusherMessage) {
        let Some(channel) = self.channel_from_pusher(msg.channel.as_deref()) else {
            return;
        };

        let Some(payload) = Self::parse_data::<KickPinnedCreatedPayload>(&msg.data) else {
            warn!("Failed to parse Kick pinned message payload");
            return;
        };

        let creator = payload
            .pinned_by
            .as_ref()
            .and_then(|u| u.username.clone().or_else(|| u.slug.clone()))
            .unwrap_or_else(|| "Kick".to_owned());
        let creator_login = creator.to_ascii_lowercase();
        let creator_user_id = payload
            .pinned_by
            .as_ref()
            .and_then(|u| u.id.map(|id| id.to_string()))
            .unwrap_or_default();

        let pinned_id = payload
            .message
            .as_ref()
            .and_then(|m| m.id.clone())
            .filter(|id| !id.trim().is_empty());
        let pinned_text = payload
            .message
            .as_ref()
            .and_then(|m| m.text.clone().or_else(|| m.content.clone()))
            .unwrap_or_default();

        let timestamp = payload
            .created_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(Utc::now);

        let chat_msg = ChatMessage {
            id: self.alloc_id(),
            server_id: pinned_id.map(|id| format!("kick:pinned:{id}")),
            timestamp,
            channel: channel.clone(),
            sender: Sender {
                user_id: UserId(creator_user_id),
                login: creator_login,
                display_name: creator,
                color: None,
                name_paint: None,
                badges: Vec::new(),
            },
            raw_text: pinned_text,
            spans: smallvec::SmallVec::new(),
            twitch_emotes: Vec::new(),
            flags: MessageFlags {
                is_action: false,
                is_highlighted: false,
                is_deleted: false,
                is_first_msg: false,
                is_pinned: true,
                is_self: false,
                is_mention: false,
                custom_reward_id: None,
                is_history: false,
            },
            reply: None,
            msg_kind: MsgKind::Chat,
        };

        self.emit(KickEvent::ChatMessage(chat_msg)).await;
    }

    async fn handle_pinned_message_deleted(&mut self, msg: &PusherMessage) {
        let Some(channel) = self.channel_from_pusher(msg.channel.as_deref()) else {
            return;
        };

        let payload = Self::parse_data::<KickPinnedDeletedPayload>(&msg.data);
        let pinned_id = payload
            .as_ref()
            .and_then(|p| {
                p.message
                    .as_ref()
                    .and_then(|m| m.id.clone())
                    .or_else(|| p.message_id.clone())
                    .or_else(|| p.id.clone())
            })
            .filter(|id| !id.trim().is_empty());

        if let Some(id) = pinned_id {
            self.emit(KickEvent::MessageDeleted {
                channel: channel.clone(),
                server_id: format!("kick:pinned:{id}"),
            })
            .await;
        }

        self.emit(KickEvent::SystemNotice(SystemNotice {
            channel: Some(channel),
            text: "The pinned message was unpinned.".to_owned(),
            timestamp: Utc::now(),
        }))
        .await;
    }
}

fn kick_badge_name(v: &serde_json::Value) -> Option<String> {
    let raw = find_deep_string(v, &["type", "badge_type", "name", "slug", "id"])?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_lowercase())
    }
}

fn kick_badge_version(v: &serde_json::Value) -> String {
    if let Some(text) = find_deep_string(v, &["text", "version", "level"]) {
        let t = text.trim();
        if !t.is_empty() {
            return t.to_owned();
        }
    }
    if let Some(val) = find_deep_value(v, &["count", "months", "tier"]) {
        if let Some(n) = val.as_u64() {
            return n.to_string();
        }
        if let Some(s) = val.as_str() {
            let t = s.trim();
            if !t.is_empty() {
                return t.to_owned();
            }
        }
    }
    "1".to_owned()
}

fn kick_badge_url(v: &serde_json::Value) -> Option<String> {
    for key in [
        "badge_url",
        "image_url",
        "icon_url",
        "url",
        "src",
        "1x",
        "2x",
        "4x",
        "small",
        "medium",
        "large",
    ] {
        if let Some(raw) = find_deep_string(v, &[key]) {
            let raw = raw.trim();
            if !raw.is_empty() && looks_like_url(raw) {
                return Some(normalize_kick_asset_url(raw));
            }
        }
    }
    None
}

fn resolve_kick_badge_fallback_url(
    badge_map: &HashMap<String, String>,
    badge_name: &str,
    version: &str,
) -> Option<String> {
    let exact = format!("{badge_name}:{version}");
    if let Some(url) = badge_map.get(&exact) {
        return Some(url.clone());
    }
    if let Some(url) = badge_map.get(badge_name) {
        return Some(url.clone());
    }

    // Subscriber badges are usually tiered by month count.
    if badge_name == "subscriber" {
        if let Ok(target) = version.parse::<u32>() {
            let mut best: Option<(u32, &String)> = None;
            for (k, v) in badge_map {
                if let Some(months_str) = k.strip_prefix("subscriber:") {
                    if let Ok(months) = months_str.parse::<u32>() {
                        if months <= target && best.map(|(m, _)| months > m).unwrap_or(true) {
                            best = Some((months, v));
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

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with("//")
        || s.starts_with('/')
}

fn find_deep_string<'a>(v: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    find_deep_value(v, keys).and_then(|val| val.as_str())
}

fn find_deep_value<'a>(v: &'a serde_json::Value, keys: &[&str]) -> Option<&'a serde_json::Value> {
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map {
                if keys.iter().any(|want| k.eq_ignore_ascii_case(want)) {
                    match val {
                        serde_json::Value::String(s) if s.trim().is_empty() => {}
                        _ => return Some(val),
                    }
                }
            }
            for val in map.values() {
                if let Some(found) = find_deep_value(val, keys) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(arr) => {
            for val in arr {
                if let Some(found) = find_deep_value(val, keys) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

fn normalize_kick_asset_url(url: &str) -> String {
    if url.starts_with("//") {
        format!("https:{url}")
    } else if url.starts_with('/') {
        format!("https://kick.com{url}")
    } else {
        url.to_owned()
    }
}

fn pusher_subscribe(channel: &str) -> String {
    serde_json::json!({
        "event": "pusher:subscribe",
        "data": { "channel": channel }
    })
    .to_string()
}

fn pusher_unsubscribe(channel: &str) -> String {
    serde_json::json!({
        "event": "pusher:unsubscribe",
        "data": { "channel": channel }
    })
    .to_string()
}

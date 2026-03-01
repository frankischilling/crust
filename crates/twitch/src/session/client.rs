use std::collections::HashSet;
use std::time::Duration;

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crust_core::model::{
    Badge, ChannelId, ChatMessage, MessageFlags, MessageId, Sender, SystemNotice, UserId,
};

use crate::{
    irc::{split_and_parse, IrcMessage},
    TwitchError,
};

const WS_URL: &str = "wss://irc-ws.chat.twitch.tv:443";
const BACKOFF_SECS: &[u64] = &[1, 2, 5, 10, 30];

// ─── Events emitted by the session ──────────────────────────────────────────

/// Events produced by the Twitch IRC session (consumed by the app reducer).
#[derive(Debug, Clone)]
pub enum TwitchEvent {
    Connected,
    Disconnected,
    Reconnecting { attempt: u32 },
    ChatMessage(ChatMessage),
    MessageDeleted {
        channel: ChannelId,
        server_id: String,
    },
    /// ROOMSTATE received — carries the Twitch numeric user-id for the channel.
    RoomState {
        channel: ChannelId,
        room_id: String,
    },
    /// Authenticated: the server confirmed our identity.
    Authenticated {
        username: String,
        user_id: String,
    },
    SystemNotice(SystemNotice),
    Error(String),
}

// ─── Commands consumed by the session ────────────────────────────────────────

#[derive(Debug)]
pub enum SessionCommand {
    JoinChannel(ChannelId),
    LeaveChannel(ChannelId),
    /// Send a PRIVMSG to a channel (requires auth).
    SendMessage(ChannelId, String),
    /// Re-connect with authentication credentials.
    Authenticate { token: String, nick: String },
    /// Drop auth and reconnect anonymously.
    LogoutAndReconnect,
    Disconnect,
}

// ─── TwitchSession ───────────────────────────────────────────────────────────

/// Twitch IRC session — supports both anonymous (justinfan) and authenticated modes.
pub struct TwitchSession {
    channels: HashSet<ChannelId>,
    event_tx: mpsc::Sender<TwitchEvent>,
    cmd_rx: mpsc::Receiver<SessionCommand>,
    next_msg_id: u64,
    /// If set, we connect with authentication.
    auth_token: Option<String>,
    auth_nick: Option<String>,
}

impl TwitchSession {
    pub fn new(
        event_tx: mpsc::Sender<TwitchEvent>,
        cmd_rx: mpsc::Receiver<SessionCommand>,
    ) -> Self {
        Self {
            channels: HashSet::new(),
            event_tx,
            cmd_rx,
            next_msg_id: 1,
            auth_token: None,
            auth_nick: None,
        }
    }

    fn alloc_id(&mut self) -> MessageId {
        let id = self.next_msg_id;
        self.next_msg_id += 1;
        MessageId(id)
    }

    async fn emit(&self, event: TwitchEvent) {
        if self.event_tx.send(event).await.is_err() {
            warn!("Event channel closed; dropping event");
        }
    }

    /// Main entry point – runs the connect/reconnect loop.
    pub async fn run(mut self) {
        let mut attempt: u32 = 0;
        loop {
            self.emit(if attempt == 0 {
                TwitchEvent::Connected // optimistic; real Connected sent after 001
            } else {
                TwitchEvent::Reconnecting { attempt }
            })
            .await;

            match self.connect_once().await {
                Ok(should_reconnect) => {
                    if !should_reconnect {
                        info!("Session disconnected cleanly");
                        self.emit(TwitchEvent::Disconnected).await;
                        return;
                    }
                    warn!("Session ended unexpectedly, will reconnect");
                }
                Err(e) => {
                    error!("Session error: {e}");
                    self.emit(TwitchEvent::Error(e.to_string())).await;
                }
            }

            let delay = BACKOFF_SECS
                .get(attempt as usize)
                .copied()
                .unwrap_or(*BACKOFF_SECS.last().unwrap());
            warn!("Reconnecting in {delay}s…");
            tokio::time::sleep(Duration::from_secs(delay)).await;
            attempt += 1;
        }
    }

    async fn connect_once(&mut self) -> Result<bool, TwitchError> {
        let is_authed = self.auth_token.is_some();
        if is_authed {
            info!("Connecting as {} to {WS_URL}", self.auth_nick.as_deref().unwrap_or("?"));
        } else {
            info!("Connecting anonymously to {WS_URL}");
        }
        let (ws_stream, _) = connect_async(WS_URL).await?;
        let (mut sink, mut stream) = ws_stream.split();

        macro_rules! send_raw {
            ($line:expr) => {
                sink.send(Message::Text($line.into())).await?
            };
        }

        // ─── Handshake ──────────────────────────────────────────────────
        send_raw!("CAP REQ :twitch.tv/membership twitch.tv/tags twitch.tv/commands");

        if let Some(token) = &self.auth_token {
            // Authenticated login
            let pass = if token.starts_with("oauth:") {
                token.clone()
            } else {
                format!("oauth:{token}")
            };
            let nick = self.auth_nick.clone().unwrap_or_else(|| "crust_user".into());
            send_raw!(format!("PASS {pass}"));
            send_raw!(format!("NICK {nick}"));
        } else {
            // Anonymous login – no PASS needed
            let nick = format!("justinfan{}", rand_number());
            send_raw!(format!("NICK {nick}"));
        }

        // Re-join tracked channels
        let channels: Vec<_> = self.channels.iter().cloned().collect();
        for ch in &channels {
            send_raw!(format!("JOIN {}", ch.irc_name()));
        }

        // ─── Event loop ──────────────────────────────────────────────────
        loop {
            tokio::select! {
                maybe_frame = stream.next() => {
                    match maybe_frame {
                        None => {
                            warn!("WebSocket stream closed");
                            return Ok(true);
                        }
                        Some(Err(e)) => {
                            error!("WebSocket error: {e}");
                            return Err(TwitchError::WebSocket(e));
                        }
                        Some(Ok(Message::Text(txt))) => {
                            let msgs = split_and_parse(&txt);
                            for result in msgs {
                                match result {
                                    Err(e) => warn!("IRC parse: {e}"),
                                    Ok(msg) => {
                                        if let Some(reply) = self.handle_irc(&msg).await {
                                            send_raw!(reply);
                                        }
                                    }
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) => {
                            info!("Server close frame");
                            return Ok(true);
                        }
                        Some(Ok(_)) => {}
                    }
                }

                maybe_cmd = self.cmd_rx.recv() => {
                    match maybe_cmd {
                        None => {
                            info!("Command channel closed");
                            return Ok(false);
                        }
                        Some(SessionCommand::Disconnect) => {
                            let _ = sink.send(Message::Text("QUIT".into())).await;
                            return Ok(false);
                        }
                        Some(SessionCommand::JoinChannel(ch)) => {
                            self.channels.insert(ch.clone());
                            send_raw!(format!("JOIN {}", ch.irc_name()));
                        }
                        Some(SessionCommand::LeaveChannel(ch)) => {
                            self.channels.remove(&ch);
                            send_raw!(format!("PART {}", ch.irc_name()));
                        }
                        Some(SessionCommand::SendMessage(ch, text)) => {
                            if self.auth_token.is_some() {
                                send_raw!(format!("PRIVMSG {} :{}", ch.irc_name(), text));
                            } else {
                                warn!("Cannot send message: not authenticated");
                            }
                        }
                        Some(SessionCommand::Authenticate { token, nick }) => {
                            info!("Auth requested, reconnecting as {nick}");
                            self.auth_token = Some(token);
                            self.auth_nick = Some(nick);
                            // Close current connection; the run() loop will reconnect with auth
                            let _ = sink.send(Message::Text("QUIT".into())).await;
                            return Ok(true); // triggers reconnect
                        }
                        Some(SessionCommand::LogoutAndReconnect) => {
                            info!("Logout requested, reconnecting anonymously");
                            self.auth_token = None;
                            self.auth_nick = None;
                            let _ = sink.send(Message::Text("QUIT".into())).await;
                            return Ok(true); // triggers reconnect
                        }
                    }
                }
            }
        }
    }

    /// Handle one IRC message. May return a raw line to send back (e.g. PONG).
    async fn handle_irc(&mut self, msg: &IrcMessage) -> Option<String> {
        match msg.command.as_str() {
            "PING" => {
                let server = msg.trailing().unwrap_or("tmi.twitch.tv");
                debug!("PING → PONG");
                return Some(format!("PONG :{server}"));
            }
            "001" => {
                let mode = if self.auth_token.is_some() { "authenticated" } else { "anonymous" };
                info!("Connected ({mode})");
                self.emit(TwitchEvent::Connected).await;
            }
            "GLOBALUSERSTATE" => {
                // Sent after successful authenticated login.
                // Extract user-id and display-name from tags.
                let user_id = msg.tags.get("user-id").unwrap_or("").to_owned();
                let display_name = msg.tags.get("display-name")
                    .filter(|s| !s.is_empty())
                    .unwrap_or(self.auth_nick.as_deref().unwrap_or(""))
                    .to_owned();
                if !user_id.is_empty() {
                    info!("Authenticated as {display_name} (user-id {user_id})");
                    self.emit(TwitchEvent::Authenticated {
                        username: display_name,
                        user_id,
                    }).await;
                }
            }
            "JOIN" => {
                if let Some(ch_raw) = msg.params.first() {
                    if self.is_own_nick(msg) {
                        let ch = ChannelId::new(ch_raw.as_str());
                        info!("Joined {ch}");
                        self.emit(TwitchEvent::SystemNotice(SystemNotice {
                            channel: Some(ch),
                            text: "Joined channel".into(),
                            timestamp: Utc::now(),
                        }))
                        .await;
                    }
                }
            }
            "PART" => {
                if self.is_own_nick(msg) {
                    if let Some(ch_raw) = msg.params.first() {
                        let ch = ChannelId::new(ch_raw.as_str());
                        self.emit(TwitchEvent::SystemNotice(SystemNotice {
                            channel: Some(ch),
                            text: "Left channel".into(),
                            timestamp: Utc::now(),
                        }))
                        .await;
                    }
                }
            }
            "PRIVMSG" => {
                if let Some(cm) = self.parse_privmsg(msg) {
                    self.emit(TwitchEvent::ChatMessage(cm)).await;
                }
            }
            "CLEARMSG" => {
                if let (Some(ch_raw), Some(target_id)) = (
                    msg.params.first(),
                    msg.tags.get("target-msg-id"),
                ) {
                    let channel = ChannelId::new(ch_raw.as_str());
                    self.emit(TwitchEvent::MessageDeleted {
                        channel,
                        server_id: target_id.to_owned(),
                    })
                    .await;
                }
            }
            "ROOMSTATE" => {
                // Extract room-id from tags and emit dedicated event
                if let Some(room_id) = msg.tags.get("room-id") {
                    if let Some(ch_raw) = msg.params.first() {
                        let ch = ChannelId::new(ch_raw.as_str());
                        self.emit(TwitchEvent::RoomState {
                            channel: ch,
                            room_id: room_id.to_owned(),
                        })
                        .await;
                    }
                }
            }
            "NOTICE" | "USERNOTICE" | "USERSTATE"
            | "HOSTTARGET" => {
                if let Some(text) = msg.trailing() {
                    let ch = msg.params.first().map(|s| ChannelId::new(s.as_str()));
                    self.emit(TwitchEvent::SystemNotice(SystemNotice {
                        channel: ch,
                        text: text.to_owned(),
                        timestamp: Utc::now(),
                    }))
                    .await;
                }
            }
            _ => {}
        }
        None
    }

    /// Check if an IRC message was sent by our own nick.
    fn is_own_nick(&self, msg: &IrcMessage) -> bool {
        if let Some(nick) = msg.nick() {
            if let Some(ref auth_nick) = self.auth_nick {
                return nick.eq_ignore_ascii_case(auth_nick);
            }
            return nick.starts_with("justinfan");
        }
        false
    }

    /// Parse PRIVMSG into a ChatMessage (spans left empty – filled by reducer).
    fn parse_privmsg(&mut self, msg: &IrcMessage) -> Option<ChatMessage> {
        let channel_raw = msg.params.first()?;
        let channel = ChannelId::new(channel_raw.as_str());
        let raw_text = msg.trailing()?.to_owned();

        // Handle /me ACTION
        let (text, is_action) = if raw_text.starts_with("\x01ACTION ")
            && raw_text.ends_with('\x01')
        {
            (&raw_text[8..raw_text.len() - 1], true)
        } else {
            (raw_text.as_str(), false)
        };

        let tags = &msg.tags;
        let login = msg.nick().unwrap_or("").to_owned();
        let display_name = tags.get("display-name").unwrap_or(&login).to_owned();
        let color = tags
            .get("color")
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let user_id = tags.get("user-id").unwrap_or("").to_owned();
        let server_id = tags.get("id").map(str::to_owned);

        let badges: Vec<Badge> = tags
            .get("badges")
            .unwrap_or("")
            .split(',')
            .filter(|s| !s.is_empty())
            .filter_map(|b| {
                let mut parts = b.splitn(2, '/');
                Some(Badge {
                    name: parts.next()?.to_owned(),
                    version: parts.next().unwrap_or("0").to_owned(),
                    url: None, // resolved later by the reducer via badge_map
                })
            })
            .collect();

        // Parse Twitch emote positions from the emotes tag
        let twitch_emotes = crust_core::format::parse_twitch_emotes_tag(
            tags.get("emotes").unwrap_or(""),
        );

        let sender = Sender {
            user_id: UserId(user_id),
            login,
            display_name,
            color,
            badges,
        };

        let is_own = self.is_own_nick(msg);

        Some(ChatMessage {
            id: self.alloc_id(),
            server_id,
            timestamp: Utc::now(),
            channel,
            sender,
            raw_text: text.to_owned(),
            spans: smallvec::SmallVec::new(), // filled by reducer
            twitch_emotes,
            flags: MessageFlags {
                is_action,
                is_highlighted: tags.get("msg-id") == Some("highlighted-message"),
                is_deleted: false,
                is_first_msg: tags.get("first-msg") == Some("1"),
                is_self: is_own,
                custom_reward_id: tags.get("custom-reward-id")
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
            },
        })
    }
}

/// Generate a random 5-digit number for justinfan nick.
fn rand_number() -> u32 {
    use std::time::SystemTime;
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(12345);
    // Simple LCG – doesn't need to be crypto
    (seed.wrapping_mul(1103515245).wrapping_add(12345) >> 16) % 90000 + 10000
}

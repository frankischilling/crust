use std::collections::HashSet;
use std::time::Duration;

use chrono::{TimeZone as _, Utc};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crust_core::model::{
    Badge, ChannelId, ChatMessage, MessageFlags, MessageId, MsgKind, ReplyInfo, Sender,
    SystemNotice, UserId,
};

use crate::{
    irc::{split_and_parse, IrcMessage},
    TwitchError,
};

const WS_URL: &str = "wss://irc-ws.chat.twitch.tv:443";
const BACKOFF_SECS: &[u64] = &[1, 2, 5, 10, 30];

// Events emitted by the session

/// Events produced by the Twitch IRC session (consumed by the app reducer).
#[derive(Debug, Clone)]
pub enum TwitchEvent {
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
    /// ROOMSTATE received - carries the Twitch numeric user-id for the channel
    /// plus room mode settings (emote-only, slow, subs-only, etc.).
    RoomState {
        channel: ChannelId,
        room_id: String,
        emote_only: Option<bool>,
        followers_only: Option<i32>,
        slow: Option<u32>,
        subs_only: Option<bool>,
        r9k: Option<bool>,
    },
    /// Authenticated: the server confirmed our identity.
    Authenticated {
        username: String,
        user_id: String,
    },
    SystemNotice(SystemNotice),
    Error(String),
    /// A user was timed out (CLEARCHAT with ban-duration tag).
    UserTimedOut {
        channel: ChannelId,
        login: String,
        seconds: u32,
    },
    /// A user was permanently banned (CLEARCHAT without ban-duration).
    UserBanned {
        channel: ChannelId,
        login: String,
    },
    /// A moderator cleared the entire chat.
    ChatCleared {
        channel: ChannelId,
    },
    /// USERSTATE received - badges, color and mod status for the logged-in user.
    UserStateUpdated {
        channel: ChannelId,
        is_mod: bool,
        badges: Vec<Badge>,
        color: Option<String>,
    },
    /// Sub / resub / giftsub notification (USERNOTICE).
    SubAlert {
        channel: ChannelId,
        /// Display name of the subscriber or gift recipient.
        display_name: String,
        /// Cumulative months (1 for new subs).
        months: u32,
        /// "Prime" | "Tier 1" | "Tier 2" | "Tier 3"
        plan: String,
        is_gift: bool,
        /// Optional message text typed by the subscriber.
        sub_msg: String,
    },
    /// Incoming raid notification (USERNOTICE msg-id=raid).
    Raid {
        channel: ChannelId,
        display_name: String,
        viewer_count: u32,
    },
}

// Commands consumed by the session

#[derive(Debug)]
pub enum SessionCommand {
    JoinChannel(ChannelId),
    LeaveChannel(ChannelId),
    /// Send a PRIVMSG to a channel (requires auth).
    SendMessage(ChannelId, String, Option<String>), // channel, text, reply_parent_msg_id
    /// Re-connect with authentication credentials.
    Authenticate {
        token: String,
        nick: String,
    },
    /// Drop auth and reconnect anonymously.
    LogoutAndReconnect,
    Disconnect,
}

// TwitchSession: manages Twitch IRC session (anonymous and authenticated modes)

/// Twitch IRC session - supports both anonymous (justinfan) and authenticated modes.
pub struct TwitchSession {
    channels: HashSet<ChannelId>,
    event_tx: mpsc::Sender<TwitchEvent>,
    cmd_rx: mpsc::Receiver<SessionCommand>,
    next_msg_id: u64,
    /// If set, we connect with authentication.
    auth_token: Option<String>,
    auth_nick: Option<String>,
    /// Set to true when Twitch explicitly rejects our auth credentials via NOTICE.
    /// Used by `run()` to break the reconnect loop rather than retrying with a
    /// token that will never work.
    auth_failed: bool,
    /// Set to true by `Authenticate` / `LogoutAndReconnect` command handlers so
    /// the `run()` loop knows the disconnect was intentional and should reset
    /// the exponential backoff counter to zero instead of treating it as a
    /// network failure.
    voluntary_reconnect: bool,
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
            auth_failed: false,
            voluntary_reconnect: false,
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

    /// Main entry point: runs the connect/reconnect loop.
    pub async fn run(mut self) {
        let mut attempt: u32 = 0;
        // Count consecutive quick failures (connection established but reset before
        // getting any IRC data). Several in a row often indicates an auth problem.
        let mut quick_fail_streak: u32 = 0;
        loop {
            self.emit(if attempt == 0 {
                TwitchEvent::Connected // optimistic; real Connected sent after 001
            } else {
                TwitchEvent::Reconnecting { attempt }
            })
            .await;

            let started = std::time::Instant::now();
            match self.connect_once().await {
                Ok(should_reconnect) => {
                    // If Twitch explicitly rejected our credentials via NOTICE,
                    // stop retrying - the token is definitively invalid.
                    if self.auth_failed {
                        let msg = "Authentication failed: Twitch rejected the token. \
                                   Please re-login with a valid token.";
                        error!("{msg}");
                        self.auth_failed = false;
                        self.auth_token = None;
                        self.emit(TwitchEvent::Error(msg.to_owned())).await;
                        // Fall through to reconnect loop but now anonymous.
                    }
                    quick_fail_streak = 0;
                    if !should_reconnect {
                        info!("Session disconnected cleanly");
                        self.emit(TwitchEvent::Disconnected).await;
                        return;
                    }
                    // Voluntary reconnect (account switch / logout): reset the
                    // backoff counter so the user is not penalised with a long
                    // wait after previous network failures.
                    if self.voluntary_reconnect {
                        self.voluntary_reconnect = false;
                        attempt = 0;
                        quick_fail_streak = 0;
                        info!("Voluntary reconnect - backoff reset, reconnecting immediately");
                        continue;
                    }
                    warn!("Session ended unexpectedly, will reconnect");
                }
                Err(e) => {
                    // Check if an auth failure preceded this connection error.
                    // Twitch sometimes RSTs the TCP connection immediately after
                    // sending the "Login authentication failed" NOTICE.
                    if self.auth_failed {
                        let msg = "Authentication failed: Twitch rejected the token. \
                                   Please re-login with a valid token.";
                        error!("{msg}");
                        self.auth_failed = false;
                        self.auth_token = None;
                        quick_fail_streak = 0;
                        self.emit(TwitchEvent::Error(msg.to_owned())).await;
                        // Continue loop; next attempt will be anonymous.
                    } else {
                        // Classify: transient IO errors (network drop, server-side RST)
                        // vs non-transient errors (programming bugs, bad URL, etc.).
                        let is_transient = is_transient_error(&e);
                        let elapsed = started.elapsed();

                        if is_transient {
                            // Connection was reset before any data was exchanged (< 3s)
                            // - could be auth rejection, rate limit, or flaky network.
                            if elapsed < Duration::from_secs(3) {
                                quick_fail_streak += 1;
                            } else {
                                quick_fail_streak = 0;
                            }
                            warn!("Connection error (attempt {attempt}): {e}");
                            // Surface a real error after several quick consecutive failures
                            // so the user knows something persistent is wrong.
                            if quick_fail_streak >= 4 {
                                let msg = format!(
                                    "Connection keeps failing - check your token or network. ({})",
                                    e
                                );
                                warn!("{msg}");
                                self.emit(TwitchEvent::Error(msg)).await;
                                quick_fail_streak = 0;
                            }
                        } else {
                            // Non-transient: surface immediately.
                            quick_fail_streak = 0;
                            error!("Session error: {e}");
                            self.emit(TwitchEvent::Error(e.to_string())).await;
                        }
                    }
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
            info!(
                "Connecting as {} to {WS_URL}",
                self.auth_nick.as_deref().unwrap_or("?")
            );
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

        // Handshake: capability negotiation and authentication
        send_raw!("CAP REQ :twitch.tv/membership twitch.tv/tags twitch.tv/commands");

        if let Some(token) = &self.auth_token {
            // Authenticated login
            let pass = if token.starts_with("oauth:") {
                token.clone()
            } else {
                format!("oauth:{token}")
            };
            let nick = self
                .auth_nick
                .clone()
                .unwrap_or_else(|| "crust_user".into());
            send_raw!(format!("PASS {pass}"));
            send_raw!(format!("NICK {nick}"));
        } else {
            // Anonymous login – no PASS needed
            let nick = format!("justinfan{}", rand_number());
            send_raw!(format!("NICK {nick}"));
        }

        // Event loop: handle incoming frames and commands
        loop {
            tokio::select! {
                maybe_frame = stream.next() => {
                    match maybe_frame {
                        None => {
                            warn!("WebSocket stream closed");
                            return Ok(true);
                        }
                        Some(Err(e)) => {
                            warn!("WebSocket read error: {e}");
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
                        Some(SessionCommand::SendMessage(ch, text, reply_id)) => {
                            if self.auth_token.is_some() {
                                let line = if let Some(parent_id) = reply_id {
                                    format!("@reply-parent-msg-id={} PRIVMSG {} :{}", parent_id, ch.irc_name(), text)
                                } else {
                                    format!("PRIVMSG {} :{}", ch.irc_name(), text)
                                };
                                send_raw!(line);
                            } else {
                                warn!("Cannot send message: not authenticated");
                            }
                        }
                        Some(SessionCommand::Authenticate { token, nick }) => {
                            info!("Auth requested, reconnecting as {nick}");
                            self.auth_token = Some(token);
                            self.auth_nick = Some(nick);
                            self.auth_failed = false; // fresh credentials, clear any previous rejection
                            self.voluntary_reconnect = true; // tell run() to reset backoff
                            // Close current connection; the run() loop will reconnect with auth
                            let _ = sink.send(Message::Text("QUIT".into())).await;
                            return Ok(true); // triggers reconnect
                        }
                        Some(SessionCommand::LogoutAndReconnect) => {
                            info!("Logout requested, reconnecting anonymously");
                            self.auth_token = None;
                            self.auth_nick = None;
                            self.auth_failed = false;
                            self.voluntary_reconnect = true; // tell run() to reset backoff
                            let _ = sink.send(Message::Text("QUIT".into())).await;
                            return Ok(true); // triggers reconnect
                        }
                    }
                }
            }
        }
    }

    /// Handle one IRC message. May return a raw line to send back (e.g., PONG).
    async fn handle_irc(&mut self, msg: &IrcMessage) -> Option<String> {
        match msg.command.as_str() {
            "PING" => {
                let server = msg.trailing().unwrap_or("tmi.twitch.tv");
                debug!("PING → PONG");
                return Some(format!("PONG :{server}"));
            }
            "001" => {
                let mode = if self.auth_token.is_some() {
                    "authenticated"
                } else {
                    "anonymous"
                };
                info!("Connected ({mode})");
                self.emit(TwitchEvent::Connected).await;
            }
            "GLOBALUSERSTATE" => {
                // Sent after successful authenticated login.
                // Extract user-id and display-name from tags.
                let user_id = msg.tags.get("user-id").unwrap_or("").to_owned();
                let display_name = msg
                    .tags
                    .get("display-name")
                    .filter(|s| !s.is_empty())
                    .unwrap_or(self.auth_nick.as_deref().unwrap_or(""))
                    .to_owned();
                if !user_id.is_empty() {
                    self.auth_failed = false; // token was accepted
                    info!("Authenticated as {display_name} (user-id {user_id})");
                    self.emit(TwitchEvent::Authenticated {
                        username: display_name,
                        user_id,
                    })
                    .await;
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
                if let (Some(ch_raw), Some(target_id)) =
                    (msg.params.first(), msg.tags.get("target-msg-id"))
                {
                    let channel = ChannelId::new(ch_raw.as_str());
                    self.emit(TwitchEvent::MessageDeleted {
                        channel,
                        server_id: target_id.to_owned(),
                    })
                    .await;
                }
            }
            "CLEARCHAT" => {
                if let Some(ch_raw) = msg.params.first() {
                    let channel = ChannelId::new(ch_raw.as_str());
                    // Trailing text present = target login was named
                    if let Some(login) = msg.trailing().filter(|s| !s.is_empty()) {
                        if let Some(secs_str) = msg.tags.get("ban-duration") {
                            let seconds = secs_str.parse::<u32>().unwrap_or(0);
                            self.emit(TwitchEvent::UserTimedOut {
                                channel,
                                login: login.to_owned(),
                                seconds,
                            })
                            .await;
                        } else {
                            self.emit(TwitchEvent::UserBanned {
                                channel,
                                login: login.to_owned(),
                            })
                            .await;
                        }
                    } else {
                        // No target: whole chat was wiped
                        self.emit(TwitchEvent::ChatCleared { channel }).await;
                    }
                }
            }
            "ROOMSTATE" => {
                // Extract room-id and room mode tags, then emit dedicated event.
                if let Some(room_id) = msg.tags.get("room-id") {
                    if let Some(ch_raw) = msg.params.first() {
                        let ch = ChannelId::new(ch_raw.as_str());
                        let emote_only = msg.tags.get("emote-only").map(|v| v == "1");
                        let followers_only = msg.tags.get("followers-only").and_then(|v| v.parse::<i32>().ok());
                        let slow = msg.tags.get("slow").and_then(|v| v.parse::<u32>().ok());
                        let subs_only = msg.tags.get("subs-only").map(|v| v == "1");
                        let r9k = msg.tags.get("r9k").map(|v| v == "1");
                        self.emit(TwitchEvent::RoomState {
                            channel: ch,
                            room_id: room_id.to_owned(),
                            emote_only,
                            followers_only,
                            slow,
                            subs_only,
                            r9k,
                        })
                        .await;
                    }
                }
            }
            "USERSTATE" => {
                // Fired after every send and on channel join.
                // Extract mod status, badges, and color.
                if let Some(ch_raw) = msg.params.first() {
                    let channel = ChannelId::new(ch_raw.as_str());
                    let is_mod = matches!(msg.tags.get("mod"), Some("1"));
                    let badges: Vec<Badge> = msg
                        .tags
                        .get("badges")
                        .unwrap_or("")
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .filter_map(|b| {
                            let mut parts = b.splitn(2, '/');
                            Some(Badge {
                                name: parts.next()?.to_owned(),
                                version: parts.next().unwrap_or("0").to_owned(),
                                url: None,
                            })
                        })
                        .collect();
                    let color = msg
                        .tags
                        .get("color")
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned);
                    self.emit(TwitchEvent::UserStateUpdated {
                        channel,
                        is_mod,
                        badges,
                        color,
                    })
                    .await;
                }
            }
            "NOTICE" | "HOSTTARGET" => {
                if let Some(text) = msg.trailing() {
                    let ch = msg.params.first().map(|s| ChannelId::new(s.as_str()));
                    // Detect explicit auth-failure notices from Twitch so the
                    // reconnect loop can stop rather than retrying indefinitely.
                    let lower = text.to_lowercase();
                    if self.auth_token.is_some()
                        && (lower.contains("login authentication failed")
                            || lower.contains("improperly formatted auth")
                            || lower.contains("invalid nick")
                            || lower.contains("authentication failed"))
                    {
                        warn!("Auth rejected by Twitch: {text}");
                        self.auth_failed = true;
                    }
                    self.emit(TwitchEvent::SystemNotice(SystemNotice {
                        channel: ch,
                        text: text.to_owned(),
                        timestamp: Utc::now(),
                    }))
                    .await;
                }
            }
            "USERNOTICE" => {
                let ch_opt = msg.params.first().map(|s| ChannelId::new(s.as_str()));
                let Some(channel) = ch_opt else {
                    return None;
                };
                let msg_id = msg.tags.get("msg-id").unwrap_or("");
                let display_name = msg
                    .tags
                    .get("display-name")
                    .filter(|s| !s.is_empty())
                    .or_else(|| msg.nick())
                    .unwrap_or("unknown")
                    .to_owned();
                let sub_msg = msg.trailing().unwrap_or("").to_owned();

                match msg_id {
                    "sub" | "resub" => {
                        let months = msg
                            .tags
                            .get("msg-param-cumulative-months")
                            .or_else(|| msg.tags.get("msg-param-months"))
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(1u32);
                        let plan =
                            decode_sub_plan(msg.tags.get("msg-param-sub-plan").unwrap_or("1000"));
                        self.emit(TwitchEvent::SubAlert {
                            channel,
                            display_name,
                            months,
                            plan,
                            is_gift: false,
                            sub_msg,
                        })
                        .await;
                    }
                    "subgift" | "anonsubgift" => {
                        let months = msg
                            .tags
                            .get("msg-param-months")
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(1u32);
                        let plan =
                            decode_sub_plan(msg.tags.get("msg-param-sub-plan").unwrap_or("1000"));
                        // For subgift, display-name is the gifter; recipient is msg-param-recipient-display-name
                        let recipient = msg
                            .tags
                            .get("msg-param-recipient-display-name")
                            .filter(|s| !s.is_empty())
                            .unwrap_or(&display_name)
                            .to_owned();
                        self.emit(TwitchEvent::SubAlert {
                            channel,
                            display_name: recipient,
                            months,
                            plan,
                            is_gift: true,
                            sub_msg,
                        })
                        .await;
                    }
                    "raid" => {
                        let viewer_count = msg
                            .tags
                            .get("msg-param-viewerCount")
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0u32);
                        self.emit(TwitchEvent::Raid {
                            channel,
                            display_name,
                            viewer_count,
                        })
                        .await;
                    }
                    _ => {
                        // Other USERNOTICE types (submysterygift, primepaidupgrade, etc.)
                        // Fall back to the decoded system-msg tag.
                        let text = msg
                            .tags
                            .get("system-msg")
                            .filter(|s| !s.is_empty())
                            .map(|s| unescape_irc_tag(s))
                            .or_else(|| msg.trailing().map(|s| s.to_owned()))
                            .unwrap_or_default();
                        if !text.is_empty() {
                            self.emit(TwitchEvent::SystemNotice(SystemNotice {
                                channel: Some(channel),
                                text,
                                timestamp: Utc::now(),
                            }))
                            .await;
                        }
                    }
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
        let (text, is_action) = if raw_text.starts_with("\x01ACTION ") && raw_text.ends_with('\x01')
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
        let twitch_emotes =
            crust_core::format::parse_twitch_emotes_tag(tags.get("emotes").unwrap_or(""));

        let sender = Sender {
            user_id: UserId(user_id),
            login,
            display_name,
            color,
            badges,
        };

        let is_own = self.is_own_nick(msg);

        // Bits detection
        let bits: u32 = tags.get("bits").and_then(|s| s.parse().ok()).unwrap_or(0);
        let msg_kind = if bits > 0 {
            MsgKind::Bits { amount: bits }
        } else {
            MsgKind::Chat
        };

        // Reply metadata
        let reply = if let Some(parent_id) = tags.get("reply-parent-msg-id") {
            if !parent_id.is_empty() {
                Some(ReplyInfo {
                    parent_msg_id: parent_id.to_owned(),
                    parent_user_login: tags.get("reply-parent-user-login").unwrap_or("").to_owned(),
                    parent_display_name: tags
                        .get("reply-parent-display-name")
                        .unwrap_or("")
                        .to_owned(),
                    parent_msg_body: tags.get("reply-parent-msg-body").unwrap_or("").to_owned(),
                })
            } else {
                None
            }
        } else {
            None
        };

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
                is_mention: false, // set by reducer once auth_username is known
                custom_reward_id: tags
                    .get("custom-reward-id")
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
                is_history: false,
            },
            reply,
            msg_kind,
        })
    }
}

/// Parse a single raw IRC PRIVMSG line into a ChatMessage without a live session.
/// Intended for processing history messages from external APIs.
///
/// * `id` - caller-assigned unique message ID
/// * `local_nick` - the authenticated user's login (lowercase), used to set `is_self`; pass `None` when not authenticated
pub fn parse_privmsg_irc(
    msg: &IrcMessage,
    local_nick: Option<&str>,
    id: u64,
) -> Option<ChatMessage> {
    let channel_raw = msg.params.first()?;
    let channel = ChannelId::new(channel_raw.as_str());
    let raw_text = msg.trailing()?.to_owned();

    // Handle /me ACTION
    let (text, is_action) = if raw_text.starts_with("\x01ACTION ") && raw_text.ends_with('\x01') {
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
                url: None,
            })
        })
        .collect();

    let twitch_emotes =
        crust_core::format::parse_twitch_emotes_tag(tags.get("emotes").unwrap_or(""));

    let sender = Sender {
        user_id: UserId(user_id),
        login: login.clone(),
        display_name,
        color,
        badges,
    };

    // Use tmi-sent-ts for accurate history timestamps instead of Utc::now().
    let timestamp = tags
        .get("tmi-sent-ts")
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
        .unwrap_or_else(Utc::now);

    let is_own = local_nick
        .map(|n| n.eq_ignore_ascii_case(&login))
        .unwrap_or(false);

    let reply = if let Some(parent_id) = tags.get("reply-parent-msg-id") {
        if !parent_id.is_empty() {
            Some(ReplyInfo {
                parent_msg_id: parent_id.to_owned(),
                parent_user_login: tags.get("reply-parent-user-login").unwrap_or("").to_owned(),
                parent_display_name: tags
                    .get("reply-parent-display-name")
                    .unwrap_or("")
                    .to_owned(),
                parent_msg_body: tags.get("reply-parent-msg-body").unwrap_or("").to_owned(),
            })
        } else {
            None
        }
    } else {
        None
    };

    Some(ChatMessage {
        id: MessageId(id),
        server_id,
        timestamp,
        channel,
        sender,
        raw_text: text.to_owned(),
        spans: smallvec::SmallVec::new(), // filled by caller
        twitch_emotes,
        flags: MessageFlags {
            is_action,
            is_highlighted: tags.get("msg-id") == Some("highlighted-message"),
            is_deleted: false,
            is_first_msg: tags.get("first-msg") == Some("1"),
            is_self: is_own,
            is_mention: false, // set by caller
            custom_reward_id: tags
                .get("custom-reward-id")
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            is_history: true,
        },
        reply,
        msg_kind: MsgKind::Chat,
    })
}

/// Decode a Twitch sub-plan code to a human-readable tier name.
fn decode_sub_plan(plan: &str) -> String {
    match plan {
        "Prime" => "Prime".to_owned(),
        "1000" => "Tier 1".to_owned(),
        "2000" => "Tier 2".to_owned(),
        "3000" => "Tier 3".to_owned(),
        other => other.to_owned(),
    }
}

/// Returns `true` for network-level errors that are transient and recoverable
/// (connection reset, refused, timed out, broken pipe, etc.).  These are
/// expected during normal reconnect cycles and should NOT be surfaced as
/// permanent errors in the UI.
fn is_transient_error(e: &crate::TwitchError) -> bool {
    use std::io::ErrorKind;
    use tokio_tungstenite::tungstenite::Error as WsErr;
    match e {
        crate::TwitchError::WebSocket(ws_err) => match ws_err {
            WsErr::Io(io_err) => matches!(
                io_err.kind(),
                ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::ConnectionRefused
                    | ErrorKind::BrokenPipe
                    | ErrorKind::TimedOut
                    | ErrorKind::UnexpectedEof
                    | ErrorKind::WouldBlock
            ),
            // Protocol-level close from the server is also recoverable.
            WsErr::ConnectionClosed | WsErr::AlreadyClosed => true,
            _ => false,
        },
        crate::TwitchError::Io(io_err) => matches!(
            io_err.kind(),
            ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::ConnectionRefused
                | ErrorKind::BrokenPipe
                | ErrorKind::TimedOut
                | ErrorKind::UnexpectedEof
        ),
        _ => false,
    }
}

/// Unescape IRC tag value escaping (\\s = space, \\: = semicolon, etc.).
pub fn unescape_irc_tag(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('s') => out.push(' '),
                Some(':') => out.push(';'),
                Some('r') => out.push('\r'),
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(o) => {
                    out.push('\\');
                    out.push(o);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
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

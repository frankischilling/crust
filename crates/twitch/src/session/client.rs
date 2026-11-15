use std::collections::HashSet;
use std::time::{Duration, Instant};

use chrono::{TimeZone as _, Utc};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crust_core::model::{
    Badge, ChannelId, ChatMessage, MessageFlags, MessageId, MsgKind, ReplyInfo, Sender,
    SharedChatSource, SystemNotice, TwitchEmotePos, UserId,
};

use crate::{
    irc::{split_and_parse, IrcMessage},
    TwitchError,
};

use super::{
    outbound::{OutboundLine, OutboundQueue},
    ratelimit::{ChatRateLimiter, RateLimiter},
};

const WS_URL: &str = "wss://irc-ws.chat.twitch.tv:443";
const BACKOFF_SECS: &[u64] = &[1, 2, 5, 10, 30];
const IRC_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IRC_REGISTER_TIMEOUT: Duration = Duration::from_secs(10);
const IRC_IDLE_INTERVAL: Duration = Duration::from_secs(5);
const IRC_PONG_TIMEOUT: Duration = Duration::from_secs(5);
const IRC_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, PartialEq, Eq)]
enum RetryAction {
    Wait,
    Reconnect,
    Stop,
}

fn session_timeout(message: &str) -> TwitchError {
    TwitchError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, message))
}

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
    /// Incoming or echoed whisper message (WHISPER).
    Whisper {
        generation: u64,
        from_user_id: String,
        from_login: String,
        from_display_name: String,
        target_login: String,
        text: String,
        twitch_emotes: Vec<TwitchEmotePos>,
        is_self: bool,
        timestamp: chrono::DateTime<Utc>,
    },
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
        generation: u64,
    },
    AuthenticationFailed {
        generation: u64,
        message: String,
    },
    SystemNotice(SystemNotice),
    Error(String),
    /// A user was timed out (CLEARCHAT with ban-duration tag).
    UserTimedOut {
        channel: ChannelId,
        login: String,
        seconds: u32,
        user_id: Option<String>,
        timestamp: Option<chrono::DateTime<Utc>>,
    },
    /// A user was permanently banned (CLEARCHAT without ban-duration).
    UserBanned {
        channel: ChannelId,
        login: String,
        user_id: Option<String>,
        timestamp: Option<chrono::DateTime<Utc>>,
    },
    /// A moderator cleared the entire chat.
    ChatCleared {
        channel: ChannelId,
        timestamp: Option<chrono::DateTime<Utc>>,
    },
    /// USERSTATE received - badges, color and mod status for the logged-in user.
    UserStateUpdated {
        channel: ChannelId,
        is_mod: bool,
        badges: Vec<Badge>,
        color: Option<String>,
        generation: u64,
    },
    /// Incoming raid notification (USERNOTICE msg-id=raid).
    Raid {
        channel: ChannelId,
        display_name: String,
        viewer_count: u32,
        source_login: Option<String>,
    },
}

// Commands consumed by the session

#[derive(Debug)]
pub enum SessionCommand {
    JoinChannel(ChannelId),
    LeaveChannel(ChannelId),
    /// Send a PRIVMSG to a channel (requires auth).
    /// The receipt reports a socket write, not server acceptance.
    SendMessage(ChannelId, String, Option<String>, oneshot::Sender<Result<(), String>>),
    /// Re-connect with authentication credentials.
    Authenticate {
        token: String,
        nick: String,
        generation: u64,
    },
    /// Drop auth and reconnect anonymously.
    LogoutAndReconnect,
    Disconnect,
}

// TwitchSession: manages Twitch IRC session (anonymous and authenticated modes)

/// Twitch IRC session - supports both anonymous (justinfan) and authenticated modes.
pub struct TwitchSession {
    // Retain sent timestamps across reconnects and account changes. Changing
    // sockets must not reset a Twitch rate window.
    chat_limiter: ChatRateLimiter,
    join_limiter: RateLimiter,
    elevated_channels: HashSet<ChannelId>,
    event_tx: mpsc::Sender<TwitchEvent>,
    cmd_rx: mpsc::Receiver<SessionCommand>,
    next_msg_id: u64,
    /// If set, we connect with authentication.
    auth_token: Option<String>,
    auth_nick: Option<String>,
    auth_generation: u64,
    connection_nick: Option<String>,
    /// Set to true when Twitch explicitly rejects our auth credentials via NOTICE.
    /// Used by `run()` to break the reconnect loop rather than retrying with a
    /// token that will never work.
    auth_failed: bool,
    /// Set to true by `Authenticate` / `LogoutAndReconnect` command handlers so
    /// the `run()` loop knows the disconnect was intentional and should reset
    /// the exponential backoff counter to zero instead of treating it as a
    /// network failure.
    voluntary_reconnect: bool,
    registered: bool,
    pending_joins: OutboundQueue,
}

impl TwitchSession {
    pub fn new(
        event_tx: mpsc::Sender<TwitchEvent>,
        cmd_rx: mpsc::Receiver<SessionCommand>,
    ) -> Self {
        Self {
            chat_limiter: ChatRateLimiter::new(),
            join_limiter: RateLimiter::join(),
            elevated_channels: HashSet::new(),
            event_tx,
            cmd_rx,
            next_msg_id: 1,
            auth_token: None,
            auth_nick: None,
            auth_generation: 0,
            connection_nick: None,
            auth_failed: false,
            voluntary_reconnect: false,
            registered: false,
            pending_joins: OutboundQueue::default(),
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

    async fn outbound_notice(&self, channel: ChannelId, text: &str) {
        self.emit(TwitchEvent::SystemNotice(SystemNotice {
            channel: Some(channel),
            text: text.to_owned(),
            timestamp: Utc::now(),
        }))
        .await;
    }

    /// Main entry point: runs the connect/reconnect loop.
    pub async fn run(self) {
        self.run_at(WS_URL).await;
    }

    async fn run_at(mut self, url: &str) {
        let mut attempt: u32 = 0;
        // Count consecutive quick failures (connection established but reset before
        // getting any IRC data). Several in a row often indicates an auth problem.
        let mut quick_fail_streak: u32 = 0;
        loop {
            let started = std::time::Instant::now();
            match self.connect_to(url).await {
                Ok(should_reconnect) => {
                    // If Twitch explicitly rejected our credentials via NOTICE,
                    // stop retrying - the token is definitively invalid.
                    if self.auth_failed {
                        let msg = "Authentication failed: Twitch rejected the token. \
                                   Please re-login with a valid token.";
                        error!("{msg}");
                        self.auth_failed = false;
                        self.auth_token = None;
                        self.auth_nick = None;
                        self.emit(TwitchEvent::AuthenticationFailed {
                            generation: self.auth_generation,
                            message: msg.to_owned(),
                        }).await;
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
                        self.auth_nick = None;
                        quick_fail_streak = 0;
                        self.emit(TwitchEvent::AuthenticationFailed {
                            generation: self.auth_generation,
                            message: msg.to_owned(),
                        }).await;
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

            if self.registered {
                attempt = 0;
                quick_fail_streak = 0;
            }
            let delay = BACKOFF_SECS
                .get(attempt as usize)
                .copied()
                .unwrap_or(*BACKOFF_SECS.last().unwrap());
            warn!("Reconnecting in {delay}s…");
            self.emit(TwitchEvent::Reconnecting { attempt: attempt.saturating_add(1) }).await;
            match self.wait_for_reconnect(Duration::from_secs(delay)).await {
                RetryAction::Wait => attempt = attempt.saturating_add(1),
                RetryAction::Reconnect => {
                    self.voluntary_reconnect = false;
                    attempt = 0;
                    quick_fail_streak = 0;
                }
                RetryAction::Stop => {
                    self.emit(TwitchEvent::Disconnected).await;
                    return;
                }
            }
        }
    }

    async fn handle_offline_command(&mut self, command: Option<SessionCommand>) -> RetryAction {
        match command {
            None | Some(SessionCommand::Disconnect) => RetryAction::Stop,
            Some(SessionCommand::Authenticate { token, nick, generation }) => {
                self.auth_token = Some(token);
                self.auth_nick = Some(nick);
                self.auth_generation = generation;
                self.auth_failed = false;
                self.voluntary_reconnect = true;
                RetryAction::Reconnect
            }
            Some(SessionCommand::LogoutAndReconnect) => {
                self.auth_token = None;
                self.auth_nick = None;
                self.auth_failed = false;
                self.voluntary_reconnect = true;
                RetryAction::Reconnect
            }
            Some(SessionCommand::JoinChannel(channel)) => {
                let channel = ChannelId::new(channel.as_str());
                if !is_valid_twitch_channel(&channel) {
                    self.outbound_notice(channel, "Channel not joined: invalid Twitch channel.").await;
                } else if self.pending_joins.join(channel.clone()).is_err() {
                    self.outbound_notice(channel, "Channel not joined: too many joins are waiting. Try again shortly.").await;
                }
                RetryAction::Wait
            }
            Some(SessionCommand::LeaveChannel(channel)) => {
                self.pending_joins.leave(&ChannelId::new(channel.as_str()));
                RetryAction::Wait
            }
            Some(SessionCommand::SendMessage(_, _, _, receipt)) => {
                let _ = receipt.send(Err("Chat is reconnecting. Try again once connected.".into()));
                RetryAction::Wait
            }
        }
    }

    async fn wait_for_reconnect(&mut self, delay: Duration) -> RetryAction {
        let deadline = tokio::time::Instant::now() + delay;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return RetryAction::Wait,
                command = self.cmd_rx.recv() => {
                    let action = self.handle_offline_command(command).await;
                    if action != RetryAction::Wait {
                        return action;
                    }
                }
            }
        }
    }

    async fn connect_to(&mut self, url: &str) -> Result<bool, TwitchError> {
        self.registered = false;
        let is_authed = self.auth_token.is_some();
        if is_authed {
            info!(
                "Connecting as {} to {WS_URL}",
                self.auth_nick.as_deref().unwrap_or("?")
            );
        } else {
            info!("Connecting anonymously to {WS_URL}");
        }
        let connecting = tokio::time::timeout(IRC_CONNECT_TIMEOUT, connect_async(url));
        tokio::pin!(connecting);
        let (ws_stream, _) = loop {
            tokio::select! {
                result = &mut connecting => {
                    break result.map_err(|_| session_timeout("Twitch IRC connection timed out"))??;
                }
                command = self.cmd_rx.recv() => {
                    match self.handle_offline_command(command).await {
                        RetryAction::Wait => {}
                        RetryAction::Reconnect => return Ok(true),
                        RetryAction::Stop => return Ok(false),
                    }
                }
            }
        };
        let (mut sink, mut stream) = ws_stream.split();

        macro_rules! send_raw {
            ($line:expr) => {
                tokio::time::timeout(IRC_WRITE_TIMEOUT, sink.send(Message::Text($line.into())))
                    .await
                    .map_err(|_| session_timeout("Twitch IRC write timed out"))??
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
            self.connection_nick = Some(nick.clone());
            send_raw!(format!("PASS {pass}"));
            send_raw!(format!("NICK {nick}"));
        } else {
            // Anonymous login - no PASS needed
            let nick = format!("justinfan{}", rand_number());
            self.connection_nick = Some(nick.clone());
            send_raw!(format!("NICK {nick}"));
        }

        // Privileges must be confirmed again for this connection. The token
        // owner's own channel is the one exception we can identify locally.
        self.elevated_channels.clear();
        if self.auth_token.is_some() {
            if let Some(nick) = self.auth_nick.as_deref() {
                let channel = ChannelId::new(nick);
                if is_valid_twitch_channel(&channel) {
                    self.elevated_channels.insert(channel);
                }
            }
        }
        let mut outbound = std::mem::take(&mut self.pending_joins);
        let registration_deadline = tokio::time::Instant::now() + IRC_REGISTER_TIMEOUT;
        let mut heartbeat_deadline = tokio::time::Instant::now() + IRC_IDLE_INTERVAL;
        let mut awaiting_pong = false;

        // Rate waits are a select branch, so incoming PINGs and account/control
        // commands remain responsive while output is queued.
        loop {
            let now = Instant::now();
            let next_send = outbound.next_send_at(
                &mut self.chat_limiter,
                &mut self.join_limiter,
                &self.elevated_channels,
                now,
            );
            let send_deadline = tokio::time::Instant::from_std(next_send.unwrap_or(now));
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
                                        heartbeat_deadline = tokio::time::Instant::now() + IRC_IDLE_INTERVAL;
                                        awaiting_pong = false;
                                        if msg.command == "RECONNECT" {
                                            info!("Server requested a reconnect");
                                            self.voluntary_reconnect = true;
                                            return Ok(true);
                                        }
                                        if let Some(reply) = self.handle_irc(&msg).await {
                                            send_raw!(reply);
                                        }
                                        if self.auth_failed {
                                            return Ok(true);
                                        }
                                    }
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) => {
                            info!("Server close frame");
                            return Ok(true);
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            tokio::time::timeout(IRC_WRITE_TIMEOUT, sink.send(Message::Pong(payload)))
                                .await
                                .map_err(|_| session_timeout("Twitch IRC write timed out"))??;
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
                            let _ = tokio::time::timeout(Duration::from_millis(250), sink.send(Message::Text("QUIT".into()))).await;
                            return Ok(false);
                        }
                        Some(SessionCommand::JoinChannel(ch)) => {
                            let ch = ChannelId::new(ch.as_str());
                            if !is_valid_twitch_channel(&ch) {
                                self.outbound_notice(ch, "Channel not joined: invalid Twitch channel.").await;
                            } else if outbound.join(ch.clone()).is_err() {
                                self.outbound_notice(ch, "Channel not joined: too many joins are waiting. Try again shortly.").await;
                            } else if self.auth_token.is_some()
                                && self.auth_nick.as_deref().is_some_and(|nick| nick.eq_ignore_ascii_case(ch.as_str()))
                            {
                                self.elevated_channels.insert(ch);
                            }
                        }
                        Some(SessionCommand::LeaveChannel(ch)) => {
                            let ch = ChannelId::new(ch.as_str());
                            if !is_valid_twitch_channel(&ch) {
                                self.outbound_notice(ch, "Channel not left: invalid Twitch channel.").await;
                            } else {
                                self.elevated_channels.remove(&ch);
                                if outbound.leave(&ch) {
                                    send_raw!(format!("PART {}", ch.irc_name()));
                                }
                            }
                        }
                        Some(SessionCommand::SendMessage(ch, text, reply_id, receipt)) => {
                            let ch = ChannelId::new(ch.as_str());
                            if self.auth_token.is_some() {
                                if let Some(line) = format_privmsg(&ch, &text, reply_id.as_deref()) {
                                    outbound.message(ch, line, Some(receipt));
                                } else {
                                    let _ = receipt.send(Err("The channel, reply ID, or message contains invalid characters.".into()));
                                }
                            } else {
                                let _ = receipt.send(Err("You are signed out. Select an account, then try again.".into()));
                            }
                        }
                        Some(SessionCommand::Authenticate { token, nick, generation }) => {
                            info!("Auth requested, reconnecting as {nick}");
                            self.auth_token = Some(token);
                            self.auth_nick = Some(nick);
                            self.auth_generation = generation;
                            self.auth_failed = false; // fresh credentials, clear any previous rejection
                            self.voluntary_reconnect = true; // tell run() to reset backoff
                            // Close current connection; the run() loop will reconnect with auth
                            let _ = tokio::time::timeout(Duration::from_millis(250), sink.send(Message::Text("QUIT".into()))).await;
                            return Ok(true); // triggers reconnect
                        }
                        Some(SessionCommand::LogoutAndReconnect) => {
                            info!("Logout requested, reconnecting anonymously");
                            self.auth_token = None;
                            self.auth_nick = None;
                            self.auth_failed = false;
                            self.voluntary_reconnect = true; // tell run() to reset backoff
                            let _ = tokio::time::timeout(Duration::from_millis(250), sink.send(Message::Text("QUIT".into()))).await;
                            return Ok(true); // triggers reconnect
                        }
                    }
                }

                _ = tokio::time::sleep_until(send_deadline), if next_send.is_some() => {
                    if let Some(OutboundLine { line, receipt }) = outbound.pop_ready(
                        &mut self.chat_limiter,
                        &mut self.join_limiter,
                        &self.elevated_channels,
                        Instant::now(),
                    ) {
                        send_raw!(line);
                        if let Some(receipt) = receipt {
                            let _ = receipt.send(Ok(()));
                        }
                    }
                }
                _ = tokio::time::sleep_until(registration_deadline), if !self.registered => {
                    return Err(session_timeout("Twitch IRC registration timed out"));
                }
                _ = tokio::time::sleep_until(heartbeat_deadline), if self.registered => {
                    if awaiting_pong {
                        return Err(session_timeout("Twitch IRC heartbeat timed out"));
                    }
                    send_raw!("PING :crust");
                    awaiting_pong = true;
                    heartbeat_deadline = tokio::time::Instant::now() + IRC_PONG_TIMEOUT;
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
                if self.registered {
                    return None;
                }
                self.registered = true;
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
                        generation: self.auth_generation,
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
            "WHISPER" => {
                if let Some((
                    from_user_id,
                    from_login,
                    from_display_name,
                    target_login,
                    text,
                    twitch_emotes,
                    is_self,
                    timestamp,
                )) = self.parse_whisper(msg)
                {
                    self.emit(TwitchEvent::Whisper {
                        generation: self.auth_generation,
                        from_user_id,
                        from_login,
                        from_display_name,
                        target_login,
                        text,
                        twitch_emotes,
                        is_self,
                        timestamp,
                    })
                    .await;
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
                    let timestamp = msg.tags.get("tmi-sent-ts")
                        .and_then(|value| value.parse::<i64>().ok())
                        .and_then(|millis| Utc.timestamp_millis_opt(millis).single());
                    let user_id = msg.tags.get("target-user-id")
                        .filter(|id| !id.is_empty()).map(str::to_owned);
                    // Trailing text present = target login was named
                    if let Some(login) = msg.params.get(1).filter(|s| !s.is_empty()) {
                        if let Some(secs_str) = msg.tags.get("ban-duration") {
                            let seconds = secs_str.parse::<u32>().unwrap_or(0);
                            self.emit(TwitchEvent::UserTimedOut {
                                channel,
                                login: login.to_owned(),
                                seconds,
                                user_id,
                                timestamp,
                            })
                            .await;
                        } else {
                            self.emit(TwitchEvent::UserBanned {
                                channel,
                                login: login.to_owned(),
                                user_id,
                                timestamp,
                            })
                            .await;
                        }
                    } else {
                        // No target: whole chat was wiped
                        self.emit(TwitchEvent::ChatCleared { channel, timestamp }).await;
                    }
                }
            }
            "ROOMSTATE" => {
                // Extract room-id and room mode tags, then emit dedicated event.
                if let Some(room_id) = msg.tags.get("room-id") {
                    if let Some(ch_raw) = msg.params.first() {
                        let ch = ChannelId::new(ch_raw.as_str());
                        let emote_only = msg.tags.get("emote-only").map(|v| v == "1");
                        let followers_only = msg
                            .tags
                            .get("followers-only")
                            .and_then(|v| v.parse::<i32>().ok());
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
                    let elevated = is_mod
                        || msg.tags.get("vip") == Some("1")
                        || badges.iter().any(|badge| {
                            matches!(
                                badge.name.as_str(),
                                "broadcaster" | "moderator" | "lead_moderator" | "vip"
                            )
                        })
                        || self
                            .auth_nick
                            .as_deref()
                            .is_some_and(|nick| nick.eq_ignore_ascii_case(channel.as_str()));
                    if self.auth_token.is_some() && elevated {
                        self.elevated_channels.insert(channel.clone());
                    } else {
                        self.elevated_channels.remove(&channel);
                    }
                    self.emit(TwitchEvent::UserStateUpdated {
                        channel,
                        is_mod,
                        badges,
                        color,
                        generation: self.auth_generation,
                    })
                    .await;
                }
            }
            "NOTICE" | "HOSTTARGET" => {
                if let Some(text) = msg.params.get(1) {
                    let ch = msg.params.first().map(|s| ChannelId::new(s.as_str()));
                    debug!(channel = ?ch, message_id = ?msg.tags.get("msg-id"), "Twitch channel notice");
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
                match msg_id {
                    "sub" | "resub" | "subgift" | "anonsubgift" => {
                        if let Some(mut message) =
                            parse_subscription_irc(msg, self.auth_nick.as_deref(), self.next_msg_id)
                        {
                            message.id = self.alloc_id();
                            message.flags.is_history = false;
                            self.emit(TwitchEvent::ChatMessage(message)).await;
                        }
                    }
                    "raid" => {
                        let source_login = msg
                            .tags
                            .get("msg-param-login")
                            .map(str::trim)
                            .filter(|login| !login.is_empty())
                            .map(str::to_ascii_lowercase);
                        let display_name = msg
                            .tags
                            .get("msg-param-displayName")
                            .filter(|name| !name.is_empty())
                            .unwrap_or(&display_name)
                            .to_owned();
                        let viewer_count = msg
                            .tags
                            .get("msg-param-viewerCount")
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0u32);
                        self.emit(TwitchEvent::Raid {
                            channel,
                            display_name,
                            viewer_count,
                            source_login,
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
                            .map(str::to_owned)
                            .or_else(|| msg.params.get(1).cloned())
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
        msg.nick()
            .zip(
                self.connection_nick
                    .as_deref()
                    .or(self.auth_nick.as_deref()),
            )
            .is_some_and(|(nick, own_nick)| nick.eq_ignore_ascii_case(own_nick))
    }

    /// Parse PRIVMSG into a ChatMessage (spans left empty - filled by reducer).
    fn parse_privmsg(&mut self, msg: &IrcMessage) -> Option<ChatMessage> {
        let mut message = parse_privmsg_irc(msg, self.auth_nick.as_deref(), self.next_msg_id)?;
        message.id = self.alloc_id();
        message.flags.is_self = self.is_own_nick(msg);
        message.flags.is_history = false;
        Some(message)
    }

    /// Parse WHISPER into a lightweight payload for UI whisper management.
    fn parse_whisper(
        &self,
        msg: &IrcMessage,
    ) -> Option<(
        String,
        String,
        String,
        String,
        String,
        Vec<TwitchEmotePos>,
        bool,
        chrono::DateTime<Utc>,
    )> {
        let target_login = msg
            .params
            .first()?
            .trim()
            .trim_start_matches('#')
            .trim_start_matches('@')
            .to_ascii_lowercase();
        if target_login.is_empty() {
            return None;
        }

        let raw_text = msg.params.get(1)?.to_owned();
        let text = if raw_text.starts_with("\x01ACTION ") && raw_text.ends_with('\x01') {
            raw_text[8..raw_text.len() - 1].to_owned()
        } else {
            raw_text
        };

        let from_login = msg.nick().unwrap_or("").trim().to_ascii_lowercase();
        if from_login.is_empty() {
            return None;
        }

        let from_user_id = msg.tags.get("user-id").unwrap_or_default().trim().to_owned();
        let from_display_name = msg
            .tags
            .get("display-name")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(from_login.as_str())
            .to_owned();

        let twitch_emotes =
            crust_core::format::parse_twitch_emotes_tag(msg.tags.get("emotes").unwrap_or(""));

        let is_self = self.is_own_nick(msg);
        let timestamp = msg
            .tags
            .get("tmi-sent-ts")
            .and_then(|s| s.parse::<i64>().ok())
            .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
            .unwrap_or_else(Utc::now);

        Some((
            from_user_id,
            from_login,
            from_display_name,
            target_login,
            text,
            twitch_emotes,
            is_self,
            timestamp,
        ))
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
    if msg.command != "PRIVMSG" || msg.params.len() < 2 {
        return None;
    }
    parse_chat_message_irc(msg, local_nick, id)
}

fn parse_subscription_irc(
    msg: &IrcMessage,
    local_nick: Option<&str>,
    id: u64,
) -> Option<ChatMessage> {
    let notice_type = msg.tags.get("msg-id")?;
    if msg.command != "USERNOTICE"
        || !matches!(notice_type, "sub" | "resub" | "subgift" | "anonsubgift")
    {
        return None;
    }
    let mut message = parse_chat_message_irc(msg, local_nick, id)?;
    let is_gift = matches!(notice_type, "subgift" | "anonsubgift");
    let display_name = if is_gift {
        msg.tags.get("msg-param-recipient-display-name")
            .filter(|name| !name.is_empty())
            .or_else(|| msg.tags.get("msg-param-recipient-user-name"))
            .unwrap_or(&message.sender.display_name)
            .to_owned()
    } else {
        message.sender.display_name.clone()
    };
    let months = if is_gift {
        msg.tags.get("msg-param-months")
    } else {
        msg.tags.get("msg-param-cumulative-months")
            .or_else(|| msg.tags.get("msg-param-months"))
    }
    .and_then(|months| months.parse().ok())
    .unwrap_or(1);
    let gifted_to_me = is_gift && local_nick.is_some_and(|nick| {
        msg.tags.get("msg-param-recipient-user-name")
            .filter(|login| !login.is_empty())
            .unwrap_or(&display_name)
            .eq_ignore_ascii_case(nick)
    });
    message.msg_kind = MsgKind::Sub {
        display_name,
        months,
        plan: decode_sub_plan(msg.tags.get("msg-param-sub-plan").unwrap_or("1000")),
        is_gift,
        sub_msg: if message.raw_text.trim().is_empty() { String::new() } else { message.raw_text.clone() },
    };
    if message.raw_text.trim().is_empty() {
        message.raw_text = message.msg_kind.subscription_notice_text().unwrap_or_default();
    }
    message.flags.is_mention |= gifted_to_me;
    message.flags.is_highlighted |= gifted_to_me;
    Some(message)
}

fn parse_chat_message_irc(
    msg: &IrcMessage,
    local_nick: Option<&str>,
    id: u64,
) -> Option<ChatMessage> {
    let channel_raw = msg.params.first()?;
    let channel = ChannelId::new(channel_raw.as_str());
    let raw_text = msg.params.get(1).map(String::as_str).unwrap_or("");

    // Handle /me ACTION
    let (text, is_action) = if raw_text.starts_with("\x01ACTION ") && raw_text.ends_with('\x01') {
        (&raw_text[8..raw_text.len() - 1], true)
    } else {
        (raw_text, false)
    };

    let tags = &msg.tags;
    let login = if msg.command == "USERNOTICE" {
        tags.get("login").unwrap_or("")
    } else {
        msg.nick().unwrap_or("")
    }.to_owned();
    let display_name = tags
        .get("display-name")
        .filter(|name| !name.is_empty())
        .unwrap_or(&login)
        .to_owned();
    let color = tags
        .get("color")
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let user_id = tags.get("user-id").unwrap_or("").to_owned();
    let server_id = extract_server_message_id(tags);

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
        original_display_name: None,
        color,
        name_paint: None,
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
            is_highlighted: irc_msg_id_contains(tags, "highlighted-message"),
            is_deleted: tags.get("rm-deleted").is_some(),
            is_first_msg: tags.get("first-msg") == Some("1"),
            is_pinned: is_pinned_chat_message(tags),
            is_self: is_own,
            is_mention: false, // set by caller
            custom_reward_id: tags
                .get("custom-reward-id")
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            is_history: true,
            suppress_notification: false,
        },
        reply,
        msg_kind: match tags.get("bits").and_then(|s| s.parse::<u32>().ok()) {
            Some(amount) if amount > 0 => MsgKind::Bits { amount },
            _ => MsgKind::Chat,
        },
        shared: parse_shared_chat(tags),
    })
}

fn parse_shared_chat(tags: &crate::irc::types::IrcTags) -> Option<SharedChatSource> {
    let room_id = tags.get("source-room-id")?.trim();
    if room_id.is_empty() || tags.get("room-id").map(str::trim) == Some(room_id) {
        return None;
    }
    let badges = tags
        .get("source-badges")
        .unwrap_or("")
        .split(',')
        .filter_map(|badge| {
            let (name, version) = badge.split_once('/')?;
            if !matches!(name, "moderator" | "vip" | "lead_moderator") {
                return None;
            }
            Some(Badge {
                name: name.to_owned(),
                version: version.to_owned(),
                url: None,
            })
        })
        .collect();
    Some(SharedChatSource {
        room_id: room_id.to_owned(),
        source_message_id: tags
            .get("source-id")
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned),
        badges,
        ..SharedChatSource::default()
    })
}

fn irc_msg_id_contains(tags: &crate::irc::types::IrcTags, needle: &str) -> bool {
    tags.get("msg-id")
        .map(|v| v.split(';').any(|part| part.eq_ignore_ascii_case(needle)))
        .unwrap_or(false)
}

fn is_pinned_chat_message(tags: &crate::irc::types::IrcTags) -> bool {
    // Twitch Hype Chat/pinned tags can vary between providers/history sources.
    // Accept any IRC tag in the pinned-chat family plus a msg-id fallback.
    tags.0
        .keys()
        .any(|k| k.to_ascii_lowercase().starts_with("pinned-chat-"))
        || tags
            .get("msg-id")
            .map(|v| {
                v.split(';')
                    .any(|part| part.to_ascii_lowercase().contains("pinned-chat"))
            })
            .unwrap_or(false)
}

fn extract_server_message_id(tags: &crate::irc::types::IrcTags) -> Option<String> {
    ["id", "source-id"].into_iter().find_map(|key| {
        tags.get(key)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
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

fn is_valid_twitch_channel(channel: &ChannelId) -> bool {
    let name = channel.as_str();
    channel.is_twitch()
        && !name.is_empty()
        && name.len() <= 25
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn format_privmsg(channel: &ChannelId, text: &str, reply_id: Option<&str>) -> Option<String> {
    if !is_valid_twitch_channel(channel)
        || !crate::irc::types::is_single_irc_line(text)
        || reply_id.is_some_and(|id| {
            id.is_empty()
                || id
                    .chars()
                    .any(|c| c.is_whitespace() || matches!(c, '\0' | ';' | '\\'))
        })
    {
        return None;
    }
    let target = channel.irc_name();
    Some(if let Some(parent_id) = reply_id {
        format!("@reply-parent-msg-id={parent_id} PRIVMSG {target} :{text}")
    } else {
        format!("PRIVMSG {target} :{text}")
    })
}

/// Generate a random 5-digit number for justinfan nick.
fn rand_number() -> u32 {
    use std::time::SystemTime;
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(12345);
    // Simple LCG - doesn't need to be crypto
    (seed.wrapping_mul(1103515245).wrapping_add(12345) >> 16) % 90000 + 10000
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irc::parse_line;

    fn session() -> (TwitchSession, mpsc::Receiver<TwitchEvent>) {
        let (event_tx, event_rx) = mpsc::channel(8);
        let (_cmd_tx, cmd_rx) = mpsc::channel(8);
        (TwitchSession::new(event_tx, cmd_rx), event_rx)
    }

    #[test]
    fn history_messages_preserve_the_deletion_marker() {
        for (extra_tag, deleted) in [("", false), (";rm-deleted", true), (";rm-deleted=1", true)] {
            let line = format!(
                "@id=history-1;tmi-sent-ts=1700000000123{extra_tag} :alice!user@host PRIVMSG #room :removed text"
            );
            let parsed = parse_privmsg_irc(&parse_line(&line).unwrap(), None, 42).unwrap();
            assert_eq!(parsed.flags.is_deleted, deleted);
            assert_eq!(parsed.raw_text, "removed text");
            assert_eq!(parsed.server_id.as_deref(), Some("history-1"));
        }
    }

    #[tokio::test]
    async fn clearchat_without_a_target_clears_the_channel() {
        let (mut session, mut events) = session();
        let message = parse_line("@room-id=1 :tmi.twitch.tv CLEARCHAT #room").unwrap();
        session.handle_irc(&message).await;
        assert!(
            matches!(events.try_recv().unwrap(), TwitchEvent::ChatCleared { channel, .. } if channel == ChannelId::new("room"))
        );
    }

    #[tokio::test]
    async fn clearchat_targets_preserve_timeout_and_ban_semantics() {
        let (mut session, mut events) = session();
        let timeout =
            parse_line("@ban-duration=600;target-user-id=10;tmi-sent-ts=1000 :tmi.twitch.tv CLEARCHAT #room :alice").unwrap();
        session.handle_irc(&timeout).await;
        assert!(
            matches!(events.try_recv().unwrap(), TwitchEvent::UserTimedOut { login, seconds: 600, user_id, timestamp, .. }
                if login == "alice" && user_id.as_deref() == Some("10") && timestamp.map(|time| time.timestamp_millis()) == Some(1000))
        );
        let ban = parse_line(":tmi.twitch.tv CLEARCHAT #room :bob").unwrap();
        session.handle_irc(&ban).await;
        assert!(
            matches!(events.try_recv().unwrap(), TwitchEvent::UserBanned { login, user_id: None, timestamp: None, .. } if login == "bob")
        );
    }

    #[tokio::test]
    async fn subscription_without_a_body_has_no_chat_text() {
        let (mut session, mut events) = session();
        let message =
            parse_line("@msg-id=sub;display-name=Alice :tmi.twitch.tv USERNOTICE #room").unwrap();
        session.handle_irc(&message).await;
        let TwitchEvent::ChatMessage(message) = events.try_recv().unwrap() else {
            panic!("expected subscription message");
        };
        assert!(matches!(&message.msg_kind, MsgKind::Sub { sub_msg, .. } if sub_msg.is_empty()));
        assert_eq!(message.raw_text, "Alice subscribed with Tier 1!");
        assert_eq!(message.plain_text(), message.raw_text);
        assert!(!message.flags.is_history);
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn subscriptions_preserve_body_identity_timestamp_and_native_emotes() {
        let (mut session, mut events) = session();
        let raw = "@id=sub-1;msg-id=resub;login=alice;display-name=Alice;user-id=10;color=#123456;badges=subscriber/12;emotes=25:2-6;tmi-sent-ts=1700000000123;msg-param-cumulative-months=12;msg-param-sub-plan=1000 :tmi.twitch.tv USERNOTICE #room :🦀 Kappa thank you";
        session.handle_irc(&parse_line(raw).unwrap()).await;
        let TwitchEvent::ChatMessage(message) = events.try_recv().unwrap() else {
            panic!("expected subscription message");
        };
        assert_eq!(message.raw_text, "🦀 Kappa thank you");
        assert_eq!(message.server_id.as_deref(), Some("sub-1"));
        assert_eq!(message.timestamp.timestamp_millis(), 1700000000123);
        assert_eq!(message.sender.login, "alice");
        assert_eq!(message.sender.user_id.0, "10");
        assert_eq!(message.sender.badges[0].name, "subscriber");
        assert_eq!(message.twitch_emotes[0].start, 2);
        assert_eq!(message.twitch_emotes[0].end, 6);
        let spans = crust_core::format::tokenize_with_flags(
            &message.raw_text, false, &message.twitch_emotes, &|_| None,
        );
        assert!(spans.iter().any(|span| matches!(span,
            crust_core::model::Span::Emote { id, code, .. } if id == "25" && code == "Kappa")));
        assert_eq!(message.plain_text(), "Alice resubscribed with Tier 1! (12 months)\n🦀 Kappa thank you");
        assert!(events.try_recv().is_err(), "one IRC notice must produce one chat row");
    }

    #[tokio::test]
    async fn gift_mentions_use_the_recipient_login_without_replacing_the_gifter() {
        let (mut session, mut events) = session();
        session.auth_nick = Some("alice".into());
        let message = parse_line("@id=gift-1;msg-id=subgift;login=bob;display-name=Bob;user-id=20;msg-param-recipient-user-name=ALICE;msg-param-recipient-display-name=アリス :tmi.twitch.tv USERNOTICE #room").unwrap();
        session.handle_irc(&message).await;
        let TwitchEvent::ChatMessage(message) = events.try_recv().unwrap() else {
            panic!("expected gift subscription");
        };
        assert_eq!(message.sender.login, "bob");
        assert!(message.flags.is_mention && message.flags.is_highlighted);
        assert!(matches!(&message.msg_kind, MsgKind::Sub { display_name, is_gift: true, .. } if display_name == "アリス"));
    }

    #[tokio::test]
    async fn raid_preserves_source_login_and_raid_display_name() {
        let (mut session, mut events) = session();
        let message = parse_line(
            "@msg-id=raid;display-name=Fallback;msg-param-login=Raider;msg-param-displayName=RaiderDisplay;msg-param-viewerCount=42 :tmi.twitch.tv USERNOTICE #room",
        )
        .unwrap();
        session.handle_irc(&message).await;
        assert!(matches!(
            events.try_recv().unwrap(),
            TwitchEvent::Raid { display_name, viewer_count: 42, source_login, .. }
                if display_name == "RaiderDisplay" && source_login.as_deref() == Some("raider")
        ));
    }

    #[tokio::test]
    async fn system_notices_do_not_decode_tags_twice() {
        let (mut session, mut events) = session();
        let message = parse_line(
            r"@msg-id=announcement;system-msg=literal\\s\sword :tmi.twitch.tv USERNOTICE #room",
        )
        .unwrap();
        session.handle_irc(&message).await;
        assert!(
            matches!(events.try_recv().unwrap(), TwitchEvent::SystemNotice(notice) if notice.text == r"literal\s word")
        );

        let empty = parse_line("@msg-id=unknown :tmi.twitch.tv USERNOTICE #room").unwrap();
        session.handle_irc(&empty).await;
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn live_and_history_messages_keep_the_same_server_metadata() {
        let (mut session, _events) = session();
        session.auth_nick = Some("alice".into());
        let message = parse_line(r"@id=message-1;bits=100;tmi-sent-ts=1700000000123;display-name=;reply-parent-msg-id=parent;reply-parent-msg-body=hello\sworld :alice!user@host PRIVMSG #room :Cheer100 🦀").unwrap();
        let live = session.parse_privmsg(&message).unwrap();
        let history = parse_privmsg_irc(&message, Some("alice"), 10).unwrap();
        for parsed in [&live, &history] {
            assert_eq!(parsed.timestamp.timestamp_millis(), 1700000000123);
            assert_eq!(parsed.sender.display_name, "alice");
            assert_eq!(parsed.raw_text, "Cheer100 🦀");
            assert_eq!(
                parsed.reply.as_ref().unwrap().parent_msg_body,
                "hello world"
            );
            assert!(matches!(parsed.msg_kind, MsgKind::Bits { amount: 100 }));
            assert!(parsed.flags.is_self);
        }
        assert!(!live.flags.is_history);
        assert!(history.flags.is_history);
    }

    #[test]
    fn messages_without_a_body_are_ignored() {
        let (mut session, _events) = session();
        let chat = parse_line(":alice!user@host PRIVMSG #room").unwrap();
        assert!(session.parse_privmsg(&chat).is_none());
        assert!(parse_privmsg_irc(&chat, None, 1).is_none());
        let whisper = parse_line(":alice!user@host WHISPER bob").unwrap();
        assert!(session.parse_whisper(&whisper).is_none());
    }

    #[tokio::test]
    async fn whispers_preserve_sender_ids_without_guessing_from_logins() {
        let (mut session, mut events) = session();
        session.auth_nick = Some("alice".into());
        session.auth_generation = 42;
        for (raw, expected_id) in [
            ("@user-id=123;display-name=Bob :bob!user@host WHISPER alice :PersonalHello", "123"),
            (":bob!user@host WHISPER alice :PersonalHello", ""),
        ] {
            session.handle_irc(&parse_line(raw).unwrap()).await;
            assert!(matches!(events.try_recv().unwrap(), TwitchEvent::Whisper {
                generation: 42, from_user_id, from_login, target_login, text, is_self: false, ..
            } if from_user_id == expected_id && from_login == "bob"
                && target_login == "alice" && text == "PersonalHello"));
        }
    }

    #[test]
    fn shared_chat_metadata_survives_live_and_history_parsing() {
        let (mut session, _events) = session();
        let message = parse_line("@room-id=1;source-room-id=2;id=local-id;source-id=source-id;source-badges=subscriber/12,moderator/1,vip/1,lead_moderator/1 :alice!user@host PRIVMSG #room :hello").unwrap();
        let live = session.parse_privmsg(&message).unwrap();
        let history = parse_privmsg_irc(&message, None, 10).unwrap();
        for parsed in [&live, &history] {
            let shared = parsed.shared.as_ref().unwrap();
            assert_eq!(shared.room_id, "2");
            assert_eq!(shared.source_message_id.as_deref(), Some("source-id"));
            let names: Vec<_> = shared
                .badges
                .iter()
                .map(|badge| badge.name.as_str())
                .collect();
            assert_eq!(names, ["moderator", "vip", "lead_moderator"]);
            assert_eq!(parsed.server_id.as_deref(), Some("local-id"));
            assert!(!parsed.flags.suppress_notification);
        }
    }

    #[test]
    fn local_and_empty_shared_chat_sources_do_not_mark_mirrors() {
        for tags in [
            "room-id=1;source-room-id=1",
            "room-id=1;source-room-id=",
            "room-id=1",
        ] {
            let line = format!("@{tags} :alice!user@host PRIVMSG #room :hello");
            let message = parse_privmsg_irc(&parse_line(&line).unwrap(), None, 1).unwrap();
            assert!(message.shared.is_none());
        }
    }

    #[test]
    fn anonymous_identity_matches_only_the_connected_nickname() {
        let (mut session, _events) = session();
        session.connection_nick = Some("justinfan12345".into());
        let own = parse_line(":justinfan12345!user@host JOIN #room").unwrap();
        let other = parse_line(":justinfan67890!user@host JOIN #room").unwrap();
        assert!(session.is_own_nick(&own));
        assert!(!session.is_own_nick(&other));
    }

    #[test]
    fn outgoing_messages_cannot_change_irc_framing() {
        let channel = ChannelId::new("room");
        assert_eq!(
            format_privmsg(&channel, "hello 🦀", Some("parent-id")).as_deref(),
            Some("@reply-parent-msg-id=parent-id PRIVMSG #room :hello 🦀")
        );
        for text in ["hello\r\nQUIT", "hello\nQUIT", "hello\0world"] {
            assert!(format_privmsg(&channel, text, None).is_none());
        }
        for reply in [
            "parent PRIVMSG #other :bad",
            "parent;other=value",
            "parent\\svalue",
        ] {
            assert!(format_privmsg(&channel, "hello", Some(reply)).is_none());
        }
        assert!(format_privmsg(&ChannelId::new("room,other"), "hello", None).is_none());
    }

    #[test]
    fn outgoing_channel_targets_allow_only_one_twitch_login() {
        for name in ["room", "room_123", "ab"] {
            assert!(is_valid_twitch_channel(&ChannelId::new(name)));
        }
        for name in [
            "",
            "room,other",
            "room other",
            "room\r\nPRIVMSG #other :injected",
            "room\0other",
            "room:other",
            "__mentions__",
            "kick:room",
        ] {
            let channel = ChannelId::new(name);
            assert!(!is_valid_twitch_channel(&channel), "{name:?}");
            assert!(format_privmsg(&channel, "hello", None).is_none());
        }
    }

    #[tokio::test]
    async fn userstate_grants_and_revokes_elevated_send_limits() {
        let (mut session, mut events) = session();
        session.auth_token = Some("test-token".into());
        session.auth_nick = Some("alice".into());
        let channel = ChannelId::new("room");
        for (tags, expected) in [
            ("mod=1;badges=", true),
            ("mod=0;badges=vip/1", true),
            ("mod=0;badges=broadcaster/1", true),
            ("mod=0;badges=moderator/1", true),
            ("mod=0;badges=", false),
        ] {
            let line = format!("@{tags} :tmi.twitch.tv USERSTATE #room");
            session.handle_irc(&parse_line(&line).unwrap()).await;
            assert_eq!(session.elevated_channels.contains(&channel), expected);
            events.try_recv().unwrap();
        }
    }

    async fn server_text(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) -> String {
        match tokio::time::timeout(Duration::from_secs(3), socket.next())
            .await
            .expect("server receives a frame promptly")
            .expect("connection remains open")
            .expect("valid WebSocket frame")
        {
            Message::Text(text) => text,
            other => panic!("expected IRC text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn queued_chat_reaches_the_socket_in_channel_order() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (event_tx, _events) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let mut session = TwitchSession::new(event_tx, cmd_rx);
        session.auth_token = Some("test-token".into());
        session.auth_nick = Some("alice".into());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            for _ in 0..3 {
                server_text(&mut socket).await;
            }
            for text in ["first", "second", "third"] {
                let (receipt, receiver) = oneshot::channel();
                cmd_tx
                    .send(SessionCommand::SendMessage(
                        ChannelId::new("room"),
                        text.to_owned(),
                        None,
                        receipt,
                    ))
                    .await
                    .unwrap();
                assert_eq!(server_text(&mut socket).await, format!("PRIVMSG #room :{text}"));
                assert_eq!(receiver.await.unwrap(), Ok(()));
            }
            cmd_tx.send(SessionCommand::Disconnect).await.unwrap();
            assert_eq!(server_text(&mut socket).await, "QUIT");
        });
        assert!(
            !tokio::time::timeout(Duration::from_secs(6), session.connect_to(&url))
                .await
                .unwrap()
                .unwrap()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rate_waits_allow_ping_and_auth_changes_without_replaying_old_chat() {
        for switch_account in [false, true] {
            let (event_tx, _events) = mpsc::channel(32);
            let (cmd_tx, cmd_rx) = mpsc::channel(16);
            let mut session = TwitchSession::new(event_tx, cmd_rx);
            session.auth_token = Some("old-token".into());
            session.auth_nick = Some("alice".into());
            let now = Instant::now();
            for _ in 0..20 {
                assert!(session.join_limiter.try_consume_at(now));
                assert!(session.chat_limiter.try_consume_at(
                    &ChannelId::new("moderated"),
                    true,
                    now,
                ));
            }
            cmd_tx
                .send(SessionCommand::JoinChannel(ChannelId::new("room")))
                .await
                .unwrap();
            let mut receipts = Vec::new();
            for text in ["old first", "old second"] {
                let (receipt, receiver) = oneshot::channel();
                cmd_tx
                    .send(SessionCommand::SendMessage(
                        ChannelId::new("room"),
                        text.to_owned(),
                        None,
                        receipt,
                    ))
                    .await
                    .unwrap();
                receipts.push(receiver);
            }

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            let server_commands = cmd_tx.clone();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                for _ in 0..3 {
                    server_text(&mut socket).await;
                }
                socket
                    .send(Message::Text("PING :waiting".to_owned()))
                    .await
                    .unwrap();
                assert_eq!(server_text(&mut socket).await, "PONG :waiting");
                let command = if switch_account {
                    SessionCommand::Authenticate {
                        token: "new-token".to_owned(),
                        nick: "bob".to_owned(),
                        generation: 1,
                    }
                } else {
                    SessionCommand::LogoutAndReconnect
                };
                server_commands.send(command).await.unwrap();
                assert_eq!(server_text(&mut socket).await, "QUIT");
            });
            assert!(
                tokio::time::timeout(Duration::from_secs(5), session.connect_to(&url))
                    .await
                    .unwrap()
                    .unwrap()
            );
            server.await.unwrap();
            for receiver in receipts {
                assert!(!matches!(receiver.await, Ok(Ok(()))), "queued message cannot be reported as written");
            }
            assert_eq!(
                session.auth_nick.as_deref(),
                switch_account.then_some("bob")
            );

            // Remove rate waits in the fixture so any accidentally retained
            // output would be visible immediately on the next connection.
            session.chat_limiter = ChatRateLimiter::new();
            session.join_limiter = RateLimiter::join();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                for _ in 0..if switch_account { 3 } else { 2 } {
                    server_text(&mut socket).await;
                }
                socket
                    .send(Message::Text("PING :new-session".to_owned()))
                    .await
                    .unwrap();
                assert_eq!(server_text(&mut socket).await, "PONG :new-session");
                assert!(
                    tokio::time::timeout(Duration::from_millis(100), socket.next())
                        .await
                        .is_err(),
                    "old JOINs and chat must not reach the new connection"
                );
                cmd_tx.send(SessionCommand::Disconnect).await.unwrap();
                assert_eq!(server_text(&mut socket).await, "QUIT");
            });
            assert!(
                !tokio::time::timeout(Duration::from_secs(5), session.connect_to(&url))
                    .await
                    .unwrap()
                    .unwrap()
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn socket_joins_are_deduplicated_and_invalid_targets_are_never_written() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (event_tx, _events) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let mut session = TwitchSession::new(event_tx, cmd_rx);
        for name in ["room,other", "room\r\nPRIVMSG #other :injected"] {
            cmd_tx
                .send(SessionCommand::JoinChannel(ChannelId::new(name)))
                .await
                .unwrap();
            cmd_tx
                .send(SessionCommand::LeaveChannel(ChannelId::new(name)))
                .await
                .unwrap();
        }
        for _ in 0..3 {
            cmd_tx
                .send(SessionCommand::JoinChannel(ChannelId::new("room")))
                .await
                .unwrap();
        }
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            for _ in 0..2 {
                server_text(&mut socket).await;
            }
            assert_eq!(server_text(&mut socket).await, "JOIN #room");
            socket
                .send(Message::Text("PING :joined".to_owned()))
                .await
                .unwrap();
            assert_eq!(server_text(&mut socket).await, "PONG :joined");
            cmd_tx
                .send(SessionCommand::LeaveChannel(ChannelId::new("room")))
                .await
                .unwrap();
            assert_eq!(server_text(&mut socket).await, "PART #room");
            cmd_tx.send(SessionCommand::Disconnect).await.unwrap();
            assert_eq!(server_text(&mut socket).await, "QUIT");
        });
        assert!(
            !tokio::time::timeout(Duration::from_secs(5), session.connect_to(&url))
                .await
                .unwrap()
                .unwrap()
        );
        server.await.unwrap();
    }

    async fn connect_with_server_message(session: &mut TwitchSession, message: &str) -> bool {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let handshake_lines = if session.auth_token.is_some() { 3 } else { 2 };
        let message = message.to_owned();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            for _ in 0..handshake_lines {
                socket.next().await.unwrap().unwrap();
            }
            socket.send(Message::Text(message)).await.unwrap();
            let _ = release_rx.await;
        });
        let result = tokio::time::timeout(Duration::from_secs(5), session.connect_to(&url))
            .await
            .unwrap()
            .unwrap();
        let _ = release_tx.send(());
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn server_reconnect_requests_restart_the_connection_immediately() {
        let (event_tx, mut events) = mpsc::channel(8);
        let (_cmd_tx, cmd_rx) = mpsc::channel(8);
        let mut session = TwitchSession::new(event_tx, cmd_rx);
        assert!(connect_with_server_message(&mut session, ":tmi.twitch.tv RECONNECT\r\n").await);
        assert!(session.voluntary_reconnect);
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn rejected_authentication_does_not_wait_for_a_socket_close() {
        let (event_tx, mut events) = mpsc::channel(8);
        let (_cmd_tx, cmd_rx) = mpsc::channel(8);
        let mut session = TwitchSession::new(event_tx, cmd_rx);
        session.auth_token = Some("test-token".into());
        session.auth_nick = Some("alice".into());
        assert!(
            connect_with_server_message(
                &mut session,
                ":tmi.twitch.tv NOTICE * :Login authentication failed\r\n"
            )
            .await
        );
        assert!(session.auth_failed);
        assert!(matches!(
            events.try_recv().unwrap(),
            TwitchEvent::SystemNotice(_)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_wait_applies_channel_changes_and_wakes_for_account_changes() {
        let (event_tx, mut events) = mpsc::channel(8);
        let (commands, command_rx) = mpsc::channel(8);
        let mut session = TwitchSession::new(event_tx, command_rx);
        let (receipt, receiver) = oneshot::channel();
        let producer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            for command in [
                SessionCommand::JoinChannel(ChannelId::new("removed")),
                SessionCommand::JoinChannel(ChannelId::new("kept")),
                SessionCommand::LeaveChannel(ChannelId::new("removed")),
                SessionCommand::SendMessage(ChannelId::new("kept"), "offline".into(), None, receipt),
                SessionCommand::Authenticate { token: "new-token".into(), nick: "bob".into(), generation: 7 },
            ] {
                commands.send(command).await.unwrap();
            }
        });
        let start = tokio::time::Instant::now();
        assert_eq!(session.wait_for_reconnect(Duration::from_secs(30)).await, RetryAction::Reconnect);
        assert_eq!(start.elapsed(), Duration::from_secs(5));
        assert_eq!(session.auth_generation, 7);
        assert_eq!(session.auth_nick.as_deref(), Some("bob"));
        assert_eq!(session.pending_joins.pop_ready(
            &mut session.chat_limiter, &mut session.join_limiter,
            &session.elevated_channels, Instant::now(),
        ).as_ref().map(|line| line.line.as_str()), Some("JOIN #kept"));
        assert!(session.pending_joins.next_send_at(
            &mut session.chat_limiter, &mut session.join_limiter,
            &session.elevated_channels, Instant::now(),
        ).is_none());
        assert!(events.try_recv().is_err());
        assert!(receiver.await.unwrap().unwrap_err().contains("reconnecting"));
        producer.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn channel_commands_do_not_extend_the_reconnect_deadline() {
        let (event_tx, _events) = mpsc::channel(8);
        let (commands, command_rx) = mpsc::channel(8);
        let mut session = TwitchSession::new(event_tx, command_rx);
        let sender = commands.clone();
        let producer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(20)).await;
            sender.send(SessionCommand::JoinChannel(ChannelId::new("room"))).await.unwrap();
        });
        let start = tokio::time::Instant::now();
        assert_eq!(session.wait_for_reconnect(Duration::from_secs(30)).await, RetryAction::Wait);
        assert_eq!(start.elapsed(), Duration::from_secs(30));
        producer.await.unwrap();
        drop(commands);
        let start = tokio::time::Instant::now();
        assert_eq!(session.wait_for_reconnect(Duration::from_secs(30)).await, RetryAction::Stop);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test]
    async fn stalled_websocket_handshakes_allow_disconnect_logout_and_account_switch() {
        for command in [
            SessionCommand::Disconnect,
            SessionCommand::LogoutAndReconnect,
            SessionCommand::Authenticate { token: "new-token".into(), nick: "bob".into(), generation: 9 },
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            let (event_tx, _events) = mpsc::channel(8);
            let (commands, command_rx) = mpsc::channel(8);
            let mut session = TwitchSession::new(event_tx, command_rx);
            session.auth_token = Some("old-token".into());
            session.auth_nick = Some("alice".into());
            let client = tokio::spawn(async move {
                let result = session.connect_to(&url).await;
                (session, result)
            });
            let (_socket, _) = listener.accept().await.unwrap();
            let stop = matches!(command, SessionCommand::Disconnect);
            let switch = matches!(command, SessionCommand::Authenticate { .. });
            commands.send(command).await.unwrap();
            let (session, result) = tokio::time::timeout(Duration::from_secs(2), client).await.unwrap().unwrap();
            assert_eq!(result.unwrap(), !stop);
            if !stop {
                assert_eq!(session.auth_nick.as_deref(), switch.then_some("bob"));
                assert!(session.voluntary_reconnect);
            }
        }
    }

    async fn open_test_connection() -> (
        tokio::task::JoinHandle<Result<bool, TwitchError>>,
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        mpsc::Sender<SessionCommand>,
        mpsc::Receiver<TwitchEvent>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (event_tx, events) = mpsc::channel(8);
        let (commands, command_rx) = mpsc::channel(8);
        let mut session = TwitchSession::new(event_tx, command_rx);
        let client = tokio::spawn(async move { session.connect_to(&url).await });
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        for _ in 0..2 {
            server_text(&mut socket).await;
        }
        (client, socket, commands, events)
    }

    #[tokio::test]
    async fn an_open_socket_without_irc_registration_times_out() {
        let (client, mut socket, _commands, _events) = open_test_connection().await;
        socket.send(Message::Ping(vec![1])).await.unwrap();
        assert!(matches!(socket.next().await.unwrap().unwrap(), Message::Pong(_)));
        tokio::time::pause();
        tokio::time::advance(IRC_REGISTER_TIMEOUT + Duration::from_millis(1)).await;
        tokio::time::resume();
        let result = tokio::time::timeout(Duration::from_secs(2), client).await.unwrap().unwrap();
        assert!(matches!(result, Err(TwitchError::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut && error.to_string().contains("registration")));
    }

    #[tokio::test]
    async fn silent_registered_connections_are_probed_and_reconnected() {
        let (client, mut socket, _commands, mut events) = open_test_connection().await;
        socket.send(Message::Text(":tmi.twitch.tv 001 guest :Welcome\r\n".into())).await.unwrap();
        assert!(matches!(events.recv().await, Some(TwitchEvent::Connected)));
        tokio::time::pause();
        tokio::time::advance(IRC_IDLE_INTERVAL).await;
        tokio::time::resume();
        assert_eq!(server_text(&mut socket).await, "PING :crust");
        tokio::time::pause();
        tokio::time::advance(IRC_PONG_TIMEOUT + Duration::from_millis(1)).await;
        tokio::time::resume();
        let result = tokio::time::timeout(Duration::from_secs(2), client).await.unwrap().unwrap();
        assert!(matches!(result, Err(TwitchError::Io(error)) if error.to_string().contains("heartbeat")));
    }

    #[tokio::test]
    async fn irc_activity_keeps_an_idle_connection_usable() {
        let (client, mut socket, commands, mut events) = open_test_connection().await;
        socket.send(Message::Text(":tmi.twitch.tv 001 guest :Welcome\r\n".into())).await.unwrap();
        assert!(matches!(events.recv().await, Some(TwitchEvent::Connected)));
        for _ in 0..2 {
            tokio::time::pause();
            tokio::time::advance(IRC_IDLE_INTERVAL).await;
            tokio::time::resume();
            assert_eq!(server_text(&mut socket).await, "PING :crust");
            socket.send(Message::Text(":tmi.twitch.tv PONG :crust\r\nPING :barrier\r\n".into())).await.unwrap();
            assert_eq!(server_text(&mut socket).await, "PONG :barrier");
        }
        commands.send(SessionCommand::Disconnect).await.unwrap();
        assert_eq!(server_text(&mut socket).await, "QUIT");
        assert!(!client.await.unwrap().unwrap());
    }

    #[tokio::test]
    async fn authentication_and_role_events_keep_the_account_generation() {
        let (mut session, mut events) = session();
        session.auth_generation = 42;
        session.handle_irc(&parse_line("@user-id=123;display-name=Alice :tmi.twitch.tv GLOBALUSERSTATE").unwrap()).await;
        assert!(matches!(events.try_recv(), Ok(TwitchEvent::Authenticated { generation: 42, user_id, .. }) if user_id == "123"));
        session.handle_irc(&parse_line("@mod=1;badges=moderator/1 :tmi.twitch.tv USERSTATE #room").unwrap()).await;
        assert!(matches!(events.try_recv(), Ok(TwitchEvent::UserStateUpdated { generation: 42, is_mod: true, .. })));
    }

    #[tokio::test]
    async fn rejected_tokens_report_the_attempted_account_generation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (event_tx, mut events) = mpsc::channel(8);
        let (commands, command_rx) = mpsc::channel(8);
        let mut session = TwitchSession::new(event_tx, command_rx);
        session.auth_token = Some("rejected-token".into());
        session.auth_nick = Some("alice".into());
        session.auth_generation = 23;
        let client = tokio::spawn(async move { session.run_at(&url).await });
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        for _ in 0..3 {
            server_text(&mut socket).await;
        }
        socket.send(Message::Text(":tmi.twitch.tv NOTICE * :Login authentication failed\r\n".into())).await.unwrap();
        let failure = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(TwitchEvent::AuthenticationFailed { generation, .. }) = events.recv().await {
                    break generation;
                }
            }
        }).await.unwrap();
        assert_eq!(failure, 23);
        commands.send(SessionCommand::Disconnect).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), client).await.unwrap().unwrap();
    }
}

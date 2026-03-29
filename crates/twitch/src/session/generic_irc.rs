use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_native_tls::TlsConnector;
use tracing::{debug, error, info, warn};

use crust_core::model::{
    Badge, ChannelId, ChatMessage, MessageFlags, MessageId, MsgKind, Sender, SystemNotice, UserId,
    IRC_SERVER_CONTROL_CHANNEL,
};

use crate::irc::{parse_line, IrcMessage};

const BACKOFF_SECS: &[u64] = &[1, 2, 5, 10, 30];
const IRC_CMD_SIZE: usize = 64;
static IRC_MSG_ID: AtomicU64 = AtomicU64::new(20_000_000_000);

#[derive(Debug, Clone)]
pub enum GenericIrcEvent {
    Connected {
        server: String,
    },
    Disconnected {
        server: String,
    },
    Reconnecting {
        server: String,
        attempt: u32,
    },
    ChatMessage(ChatMessage),
    SystemNotice(SystemNotice),
    /// An IRC server redirected us from one channel to another (e.g. 470).
    ChannelRedirected {
        server_key: (String, u16, bool),
        old_channel: String,
        new_channel: String,
    },
    Error {
        server: String,
        message: String,
    },
    /// The channel topic was set or changed (TOPIC command or RPL_TOPIC 332).
    TopicChanged {
        channel: ChannelId,
        topic: String,
    },
}

#[derive(Debug)]
pub enum GenericIrcSessionCommand {
    JoinChannel {
        channel: ChannelId,
        key: Option<String>,
    },
    LeaveChannel(ChannelId),
    SendMessage(ChannelId, String),
    SetNick(String),
    /// Set NickServ credentials for auto-identification after connect.
    SetNickServAuth {
        nickserv_user: String,
        nickserv_pass: String,
    },
    Disconnect,
}

#[derive(Debug)]
enum ServerCommand {
    Join {
        channel: String,
        key: Option<String>,
    },
    Leave(String),
    Send {
        channel: String,
        text: String,
    },
    SetNick(String),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ServerKey {
    host: String,
    port: u16,
    tls: bool,
}

impl ServerKey {
    fn from_channel(channel: &ChannelId) -> Option<Self> {
        let t = channel.irc_target()?;
        Some(Self {
            host: t.host,
            port: t.port,
            tls: t.tls,
        })
    }

    fn label(&self) -> String {
        if self.tls {
            format!("ircs://{}:{}", self.host, self.port)
        } else {
            format!("irc://{}:{}", self.host, self.port)
        }
    }
}

struct ServerHandle {
    cmd_tx: mpsc::Sender<ServerCommand>,
    channels: HashMap<String, Option<String>>,
}

#[derive(Debug, thiserror::Error)]
enum IrcSessionError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TLS error: {0}")]
    Tls(#[from] native_tls::Error),
}

pub struct GenericIrcSession {
    event_tx: mpsc::Sender<GenericIrcEvent>,
    cmd_rx: mpsc::Receiver<GenericIrcSessionCommand>,
    preferred_nick: String,
    /// NickServ credentials for auto-identification after connect.
    nickserv_user: String,
    nickserv_pass: String,
}

impl GenericIrcSession {
    pub fn new(
        event_tx: mpsc::Sender<GenericIrcEvent>,
        cmd_rx: mpsc::Receiver<GenericIrcSessionCommand>,
    ) -> Self {
        Self {
            event_tx,
            cmd_rx,
            preferred_nick: random_guest_nick(),
            nickserv_user: String::new(),
            nickserv_pass: String::new(),
        }
    }

    async fn emit(&self, evt: GenericIrcEvent) {
        if self.event_tx.send(evt).await.is_err() {
            warn!("IRC event channel closed; dropping event");
        }
    }

    pub async fn run(mut self) {
        let mut servers: HashMap<ServerKey, ServerHandle> = HashMap::new();

        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                GenericIrcSessionCommand::JoinChannel { channel: ch, key } => {
                    if !ch.is_irc() {
                        continue;
                    }
                    let Some(target) = ch.irc_target() else {
                        self.emit(GenericIrcEvent::Error {
                            server: "irc".to_owned(),
                            message: format!("Invalid IRC channel target: {}", ch.as_str()),
                        })
                        .await;
                        continue;
                    };
                    let server_key = ServerKey::from_channel(&ch).expect("validated above");
                    let channel_name = target.channel.to_lowercase();
                    let join_key = key
                        .as_deref()
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned);

                    if let Some(handle) = servers.get_mut(&server_key) {
                        let previous = handle.channels.get(&channel_name).cloned();
                        handle
                            .channels
                            .insert(channel_name.clone(), join_key.clone());
                        if previous.is_none() || previous != Some(join_key.clone()) {
                            let _ = handle
                                .cmd_tx
                                .send(ServerCommand::Join {
                                    channel: channel_name,
                                    key: join_key,
                                })
                                .await;
                        }
                        continue;
                    }

                    let (server_cmd_tx, server_cmd_rx) =
                        mpsc::channel::<ServerCommand>(IRC_CMD_SIZE);
                    let mut channels = HashMap::new();
                    channels.insert(channel_name.clone(), join_key.clone());

                    let worker = ServerWorker::new(
                        server_key.clone(),
                        channels.clone(),
                        self.event_tx.clone(),
                        server_cmd_rx,
                        self.preferred_nick.clone(),
                        self.nickserv_user.clone(),
                        self.nickserv_pass.clone(),
                    );
                    tokio::spawn(worker.run());

                    servers.insert(
                        server_key,
                        ServerHandle {
                            cmd_tx: server_cmd_tx,
                            channels,
                        },
                    );
                }
                GenericIrcSessionCommand::LeaveChannel(ch) => {
                    if !ch.is_irc() {
                        continue;
                    }
                    let Some(target) = ch.irc_target() else {
                        continue;
                    };
                    let Some(key) = ServerKey::from_channel(&ch) else {
                        continue;
                    };

                    if let Some(mut handle) = servers.remove(&key) {
                        handle.channels.remove(&target.channel.to_lowercase());
                        let _ = handle
                            .cmd_tx
                            .send(ServerCommand::Leave(target.channel.to_lowercase()))
                            .await;
                        if handle.channels.is_empty() {
                            let _ = handle.cmd_tx.send(ServerCommand::Shutdown).await;
                        } else {
                            servers.insert(key, handle);
                        }
                    }
                }
                GenericIrcSessionCommand::SendMessage(ch, text) => {
                    if !ch.is_irc() {
                        continue;
                    }
                    let Some(target) = ch.irc_target() else {
                        continue;
                    };
                    let Some(key) = ServerKey::from_channel(&ch) else {
                        continue;
                    };
                    if let Some(handle) = servers.get(&key) {
                        let _ = handle
                            .cmd_tx
                            .send(ServerCommand::Send {
                                channel: target.channel.to_lowercase(),
                                text,
                            })
                            .await;
                    }
                }
                GenericIrcSessionCommand::SetNick(nick) => {
                    let Some(valid) = normalize_irc_nick(&nick) else {
                        self.emit(GenericIrcEvent::Error {
                            server: "irc".to_owned(),
                            message: "Invalid IRC nickname".to_owned(),
                        })
                        .await;
                        continue;
                    };
                    self.preferred_nick = valid.clone();
                    for handle in servers.values() {
                        let _ = handle
                            .cmd_tx
                            .send(ServerCommand::SetNick(valid.clone()))
                            .await;
                    }
                }
                GenericIrcSessionCommand::SetNickServAuth {
                    nickserv_user,
                    nickserv_pass,
                } => {
                    self.nickserv_user = nickserv_user;
                    self.nickserv_pass = nickserv_pass;
                }
                GenericIrcSessionCommand::Disconnect => {
                    for (_, handle) in servers.drain() {
                        let _ = handle.cmd_tx.send(ServerCommand::Shutdown).await;
                    }
                    return;
                }
            }
        }
    }
}

struct ServerWorker {
    key: ServerKey,
    channels: HashMap<String, Option<String>>,
    event_tx: mpsc::Sender<GenericIrcEvent>,
    cmd_rx: mpsc::Receiver<ServerCommand>,
    nick: String,
    pass: Option<String>,
    user: Option<String>,
    realname: Option<String>,
    /// NickServ username for auto-identification after 001.
    nickserv_user: String,
    /// NickServ password for auto-identification after 001.
    nickserv_pass: String,
}

enum WorkerExit {
    Shutdown,
    Reconnect,
}

impl ServerWorker {
    fn new(
        key: ServerKey,
        channels: HashMap<String, Option<String>>,
        event_tx: mpsc::Sender<GenericIrcEvent>,
        cmd_rx: mpsc::Receiver<ServerCommand>,
        preferred_nick: String,
        nickserv_user: String,
        nickserv_pass: String,
    ) -> Self {
        let nick = normalize_irc_nick(&preferred_nick).unwrap_or_else(random_guest_nick);
        Self {
            key,
            channels,
            event_tx,
            cmd_rx,
            nick,
            pass: None,
            user: None,
            realname: None,
            nickserv_user,
            nickserv_pass,
        }
    }

    async fn emit(&self, evt: GenericIrcEvent) {
        let _ = self.event_tx.send(evt).await;
    }

    async fn run(mut self) {
        let mut attempt = 0u32;
        loop {
            if self.channels.is_empty() {
                match self.cmd_rx.recv().await {
                    Some(ServerCommand::Join { channel, key }) => {
                        self.channels.insert(channel, key);
                        continue;
                    }
                    Some(ServerCommand::Shutdown) | None => return,
                    Some(ServerCommand::Leave(_)) => continue,
                    Some(ServerCommand::Send { .. }) => continue,
                    Some(ServerCommand::SetNick(nick)) => {
                        if let Some(valid) = normalize_irc_nick(&nick) {
                            self.nick = valid;
                        }
                        continue;
                    }
                }
            }

            if attempt > 0 {
                self.emit(GenericIrcEvent::Reconnecting {
                    server: self.key.label(),
                    attempt,
                })
                .await;
            }

            match self.connect_once().await {
                Ok(WorkerExit::Shutdown) => {
                    self.emit(GenericIrcEvent::Disconnected {
                        server: self.key.label(),
                    })
                    .await;
                    return;
                }
                Ok(WorkerExit::Reconnect) => {}
                Err(e) => {
                    error!("IRC session {} error: {e}", self.key.label());
                    self.emit(GenericIrcEvent::Error {
                        server: self.key.label(),
                        message: e.to_string(),
                    })
                    .await;
                }
            }

            let delay = BACKOFF_SECS
                .get(attempt as usize)
                .copied()
                .unwrap_or(*BACKOFF_SECS.last().unwrap());
            tokio::time::sleep(Duration::from_secs(delay)).await;
            attempt += 1;
        }
    }

    async fn connect_once(&mut self) -> Result<WorkerExit, IrcSessionError> {
        info!("Connecting to IRC server {}", self.key.label());
        let tcp = TcpStream::connect((self.key.host.as_str(), self.key.port)).await?;
        tcp.set_nodelay(true)?;

        let stream: Box<dyn AsyncReadWrite> = if self.key.tls {
            let tls = native_tls::TlsConnector::builder().build()?;
            let connector = TlsConnector::from(tls);
            let tls_stream = connector.connect(&self.key.host, tcp).await?;
            Box::new(tls_stream)
        } else {
            Box::new(tcp)
        };

        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        let mut line = Vec::<u8>::new();
        let mut ping_timer = tokio::time::interval(Duration::from_secs(60));
        ping_timer.tick().await;
        let mut connected_emitted = false;

        if let Some(pass) = self.pass.as_deref() {
            if !pass.is_empty() {
                write_irc_line(&mut write_half, &format!("PASS {pass}")).await?;
            }
        }
        write_irc_line(&mut write_half, &format!("NICK {}", self.nick)).await?;
        let user = self.user.as_deref().unwrap_or(&self.nick);
        let realname = self.realname.as_deref().unwrap_or(user);
        write_irc_line(&mut write_half, &format!("USER {user} 0 * :{realname}")).await?;

        for (ch, key) in &self.channels {
            if ch != IRC_SERVER_CONTROL_CHANNEL {
                let _ = write_join_line(&mut write_half, ch, key.as_deref()).await;
            }
        }

        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        None | Some(ServerCommand::Shutdown) => {
                            let _ = write_irc_line(&mut write_half, "QUIT :Bye").await;
                            return Ok(WorkerExit::Shutdown);
                        }
                        Some(ServerCommand::Join { channel, key }) => {
                            let previous = self.channels.insert(channel.clone(), key.clone());
                            if previous.is_none() || previous != Some(key.clone()) {
                                if channel != IRC_SERVER_CONTROL_CHANNEL {
                                    let _ = write_join_line(&mut write_half, &channel, key.as_deref()).await;
                                }
                            }
                        }
                        Some(ServerCommand::Leave(ch)) => {
                            if self.channels.remove(&ch).is_some() {
                                if ch != IRC_SERVER_CONTROL_CHANNEL {
                                    let _ = write_irc_line(&mut write_half, &format!("PART #{ch}")).await;
                                }
                                if self.channels.is_empty() {
                                    let _ = write_irc_line(&mut write_half, "QUIT :No channels left").await;
                                    return Ok(WorkerExit::Shutdown);
                                }
                            }
                        }
                        Some(ServerCommand::Send { channel, text }) => {
                            let handled = self
                                .handle_outgoing_input(&mut write_half, &channel, &text)
                                .await?;
                            if let Some(exit) = handled {
                                return Ok(exit);
                            }
                        }
                        Some(ServerCommand::SetNick(nick)) => {
                            if let Some(valid) = normalize_irc_nick(&nick) {
                                self.nick = valid.clone();
                                let _ = write_irc_line(&mut write_half, &format!("NICK {valid}")).await;
                            }
                        }
                    }
                }
                read_res = reader.read_until(b'\n', &mut line) => {
                    let n = read_res?;
                    if n == 0 {
                        warn!("IRC {} closed connection", self.key.label());
                        return Ok(WorkerExit::Reconnect);
                    }
                    let raw = String::from_utf8_lossy(&line);
                    let raw = raw.trim_end_matches(&['\r', '\n'][..]).to_owned();
                    line.clear();
                    if raw.is_empty() {
                        continue;
                    }

                    if let Ok(msg) = parse_line(&raw) {
                        if msg.command == "PING" {
                            if let Some(token) = msg.trailing() {
                                let _ = write_irc_line(&mut write_half, &format!("PONG :{token}")).await;
                            } else {
                                let _ = write_irc_line(&mut write_half, "PONG").await;
                            }
                            continue;
                        }

                        if msg.command == "001" && !connected_emitted {
                            connected_emitted = true;
                            self.emit(GenericIrcEvent::Connected {
                                server: self.key.label(),
                            }).await;

                            // Auto-identify with NickServ if credentials are configured.
                            if !self.nickserv_user.is_empty() && !self.nickserv_pass.is_empty() {
                                let identify_cmd = format!(
                                    "PRIVMSG NickServ :IDENTIFY {} {}",
                                    self.nickserv_user, self.nickserv_pass
                                );
                                let _ = write_irc_line(&mut write_half, &identify_cmd).await;
                                info!("Sent NickServ IDENTIFY for {}", self.nickserv_user);
                                // Also change nick to the identified account name.
                                if let Some(valid) = normalize_irc_nick(&self.nickserv_user) {
                                    self.nick = valid.clone();
                                    let _ = write_irc_line(&mut write_half, &format!("NICK {valid}")).await;
                                    info!("Auto nick-change to {valid} after NickServ IDENTIFY");
                                }
                            }

                            continue;
                        }

                        if msg.command == "433" {
                            self.nick = fallback_nick_after_collision(&self.nick);
                            let _ = write_irc_line(&mut write_half, &format!("NICK {}", self.nick)).await;
                            continue;
                        }

                        // IRC 470: channel redirect (e.g. #chat → ##chat on Libera).
                        // Format: :server 470 nick #oldchan ##newchan :Forwarding to another channel
                        if msg.command == "470" {
                            if msg.params.len() >= 3 {
                                // params[0] = our nick, params[1] = old channel, params[2] = new channel
                                let old_raw = msg.params[1].trim();
                                let new_raw = msg.params[2].trim();
                                if let (Some(old_ch), Some(new_ch)) = (
                                    normalize_irc_channel_name(old_raw),
                                    normalize_irc_channel_name(new_raw),
                                ) {
                                    info!(
                                        "IRC channel redirect on {}: #{} → #{}",
                                        self.key.label(), old_ch, new_ch,
                                    );
                                    // Update internal channel map: move the join key from old→new
                                    let key = self.channels.remove(&old_ch);
                                    self.channels.insert(new_ch.clone(), key.flatten());

                                    self.emit(GenericIrcEvent::ChannelRedirected {
                                        server_key: (
                                            self.key.host.clone(),
                                            self.key.port,
                                            self.key.tls,
                                        ),
                                        old_channel: old_ch,
                                        new_channel: new_ch,
                                    })
                                    .await;
                                }
                            }
                            // Still let it fall through to parse_irc_system_notice so
                            // the user sees the notice text in chat.
                        }

                        if msg.command == "PRIVMSG" {
                            if let Some(chat) = parse_irc_privmsg(&msg, &self.key, &self.nick) {
                                self.emit(GenericIrcEvent::ChatMessage(chat)).await;
                            }
                            continue;
                        }

                        // Emit a structured TopicChanged for TOPIC and RPL_TOPIC (332).
                        if msg.command == "TOPIC" {
                            if let Some(topic) = msg.trailing() {
                                if let Some(ch) = msg.params.first().and_then(|p| normalize_irc_target_channel(p)) {
                                    self.emit(GenericIrcEvent::TopicChanged {
                                        channel: ChannelId::irc(&self.key.host, self.key.port, self.key.tls, ch),
                                        topic: topic.to_owned(),
                                    }).await;
                                }
                            }
                        } else if msg.command == "332" {
                            // RPL_TOPIC: params = [nick, #channel], trailing = topic
                            if let Some(topic) = msg.trailing() {
                                if let Some(ch) = msg.params.iter().find_map(|p| normalize_irc_target_channel(p)) {
                                    self.emit(GenericIrcEvent::TopicChanged {
                                        channel: ChannelId::irc(&self.key.host, self.key.port, self.key.tls, ch),
                                        topic: topic.to_owned(),
                                    }).await;
                                }
                            }
                        }

                        if let Some(notice) =
                            parse_irc_system_notice(&msg, &self.key, &self.channels, &self.nick)
                        {
                            self.emit(GenericIrcEvent::SystemNotice(notice)).await;
                        }
                    } else {
                        debug!("Failed to parse IRC line from {}: {}", self.key.label(), raw);
                    }
                }
                _ = ping_timer.tick() => {
                    let _ = write_irc_line(&mut write_half, "PING :crust").await;
                }
            }
        }
    }

    fn command_context_channel(&self, source_channel: &str) -> Option<String> {
        if source_channel != IRC_SERVER_CONTROL_CHANNEL {
            return Some(source_channel.to_owned());
        }
        self.channels
            .keys()
            .find(|ch| ch.as_str() != IRC_SERVER_CONTROL_CHANNEL)
            .cloned()
    }

    fn notice_channel_id(&self, source_channel: &str) -> Option<ChannelId> {
        let channel_name = if source_channel != IRC_SERVER_CONTROL_CHANNEL {
            source_channel.to_owned()
        } else if self.channels.contains_key(IRC_SERVER_CONTROL_CHANNEL) {
            IRC_SERVER_CONTROL_CHANNEL.to_owned()
        } else {
            self.channels
                .keys()
                .next()
                .cloned()
                .unwrap_or_else(|| IRC_SERVER_CONTROL_CHANNEL.to_owned())
        };
        Some(ChannelId::irc(
            &self.key.host,
            self.key.port,
            self.key.tls,
            channel_name,
        ))
    }

    async fn emit_command_notice(&self, source_channel: &str, text: impl Into<String>) {
        self.emit(GenericIrcEvent::SystemNotice(SystemNotice {
            channel: self.notice_channel_id(source_channel),
            text: text.into(),
            timestamp: Utc::now(),
        }))
        .await;
    }

    async fn handle_outgoing_input<W: AsyncWrite + Unpin>(
        &mut self,
        writer: &mut W,
        source_channel: &str,
        text: &str,
    ) -> Result<Option<WorkerExit>, IrcSessionError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        if !trimmed.starts_with('/') {
            if is_raw_irc_protocol_line(trimmed) {
                let _ = write_irc_line(writer, trimmed).await;
                return Ok(None);
            }
            if source_channel != IRC_SERVER_CONTROL_CHANNEL
                && self.channels.contains_key(source_channel)
            {
                let _ =
                    write_irc_line(writer, &format!("PRIVMSG #{source_channel} :{trimmed}")).await;
            } else {
                self.emit_command_notice(
                    source_channel,
                    "Join a channel first (`/join #channel`) before sending messages.",
                )
                .await;
            }
            return Ok(None);
        }

        let cmd_line = trimmed.trim_start_matches('/').trim_start();
        if cmd_line.is_empty() {
            return Ok(None);
        }
        let (cmd, rest_raw) = cmd_line
            .split_once(char::is_whitespace)
            .map(|(c, r)| (c, r.trim_start()))
            .unwrap_or((cmd_line, ""));
        let cmd_lower = cmd.to_ascii_lowercase();

        match cmd_lower.as_str() {
            "raw" => {
                if rest_raw.is_empty() {
                    self.emit_command_notice(source_channel, "Usage: /raw <line>")
                        .await;
                } else {
                    let _ = write_irc_line(writer, rest_raw).await;
                }
            }
            "msg" | "privmsg" => {
                let mut parts = rest_raw.splitn(2, char::is_whitespace);
                let Some(target) = parts.next().map(str::trim).filter(|s| !s.is_empty()) else {
                    self.emit_command_notice(source_channel, "Usage: /msg <target> <message>")
                        .await;
                    return Ok(None);
                };
                let Some(body) = parts
                    .next()
                    .map(str::trim_start)
                    .and_then(strip_optional_irc_trailing_prefix)
                else {
                    self.emit_command_notice(source_channel, "Usage: /msg <target> <message>")
                        .await;
                    return Ok(None);
                };
                let target = normalize_irc_msg_target(target);
                let _ = write_irc_line(writer, &format!("PRIVMSG {target} :{body}")).await;

                // When sending IDENTIFY to NickServ with an explicit account
                // name, automatically attempt to change nick to that account
                // so the user doesn't have to send a separate /nick command.
                if target.eq_ignore_ascii_case("nickserv") {
                    let words: Vec<&str> = body.split_whitespace().collect();
                    if words
                        .first()
                        .map(|s| s.eq_ignore_ascii_case("identify"))
                        .unwrap_or(false)
                        && words.len() >= 3
                    {
                        // IDENTIFY <account> <password>
                        if let Some(valid) = normalize_irc_nick(words[1]) {
                            self.nick = valid.clone();
                            let _ = write_irc_line(writer, &format!("NICK {valid}")).await;
                            info!("Auto nick-change to {valid} after NickServ IDENTIFY");
                        }
                    }
                }
            }
            "notice" => {
                let mut parts = rest_raw.splitn(2, char::is_whitespace);
                let Some(target) = parts.next().map(str::trim).filter(|s| !s.is_empty()) else {
                    self.emit_command_notice(source_channel, "Usage: /notice <target> <message>")
                        .await;
                    return Ok(None);
                };
                let Some(body) = parts
                    .next()
                    .map(str::trim_start)
                    .and_then(strip_optional_irc_trailing_prefix)
                else {
                    self.emit_command_notice(source_channel, "Usage: /notice <target> <message>")
                        .await;
                    return Ok(None);
                };
                let target = normalize_irc_msg_target(target);
                let _ = write_irc_line(writer, &format!("NOTICE {target} :{body}")).await;
            }
            "join" => {
                let mut parts = rest_raw.split_whitespace();
                let Some(raw_channel) = parts.next() else {
                    self.emit_command_notice(source_channel, "Usage: /join <#channel> [key]")
                        .await;
                    return Ok(None);
                };
                let Some(channel) = normalize_irc_channel_name(raw_channel) else {
                    self.emit_command_notice(source_channel, "Invalid channel. Try /join #channel")
                        .await;
                    return Ok(None);
                };
                let key = parts
                    .next()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                self.channels.insert(channel.clone(), key.clone());
                if channel != IRC_SERVER_CONTROL_CHANNEL {
                    let _ = write_join_line(writer, &channel, key.as_deref()).await;
                }
            }
            "part" => {
                let (channel, reason) = parse_part_command_args(
                    source_channel,
                    rest_raw,
                    self.command_context_channel(source_channel).as_deref(),
                );
                let Some(channel) = channel else {
                    self.emit_command_notice(source_channel, "Usage: /part [#channel] [reason]")
                        .await;
                    return Ok(None);
                };
                if channel != IRC_SERVER_CONTROL_CHANNEL {
                    if let Some(reason) = reason {
                        let _ = write_irc_line(writer, &format!("PART #{channel} :{reason}")).await;
                    } else {
                        let _ = write_irc_line(writer, &format!("PART #{channel}")).await;
                    }
                }
                self.channels.remove(&channel);
                if self.channels.is_empty() {
                    let _ = write_irc_line(writer, "QUIT :No channels left").await;
                    return Ok(Some(WorkerExit::Shutdown));
                }
            }
            "topic" => {
                let (chan, topic) = parse_topic_command_args(
                    rest_raw,
                    self.command_context_channel(source_channel),
                );
                let Some(channel) = chan else {
                    self.emit_command_notice(source_channel, "Usage: /topic [#channel] [topic]")
                        .await;
                    return Ok(None);
                };
                if let Some(topic) = topic {
                    let _ = write_irc_line(writer, &format!("TOPIC #{channel} :{topic}")).await;
                } else {
                    let _ = write_irc_line(writer, &format!("TOPIC #{channel}")).await;
                }
            }
            "names" => {
                if rest_raw.trim().is_empty() {
                    if let Some(ch) = self.command_context_channel(source_channel) {
                        let _ = write_irc_line(writer, &format!("NAMES #{ch}")).await;
                    } else {
                        let _ = write_irc_line(writer, "NAMES").await;
                    }
                } else {
                    let _ = write_irc_line(writer, &format!("NAMES {}", rest_raw.trim())).await;
                }
            }
            "list" => {
                if rest_raw.trim().is_empty() {
                    let _ = write_irc_line(writer, "LIST").await;
                } else {
                    let _ = write_irc_line(writer, &format!("LIST {}", rest_raw.trim())).await;
                }
            }
            "mode" => {
                if rest_raw.trim().is_empty() {
                    if let Some(ch) = self.command_context_channel(source_channel) {
                        let _ = write_irc_line(writer, &format!("MODE #{ch}")).await;
                    } else {
                        self.emit_command_notice(source_channel, "Usage: /mode <target> [modes]")
                            .await;
                    }
                } else {
                    let _ = write_irc_line(writer, &format!("MODE {}", rest_raw.trim())).await;
                }
            }
            "kick" => {
                let (chan, nick, reason) = parse_kick_command_args(
                    source_channel,
                    rest_raw,
                    self.command_context_channel(source_channel).as_deref(),
                );
                let Some(channel) = chan else {
                    self.emit_command_notice(
                        source_channel,
                        "Usage: /kick <#channel> <nick> [reason]",
                    )
                    .await;
                    return Ok(None);
                };
                let Some(nick) = nick else {
                    self.emit_command_notice(
                        source_channel,
                        "Usage: /kick <#channel> <nick> [reason]",
                    )
                    .await;
                    return Ok(None);
                };
                if let Some(reason) = reason {
                    let _ =
                        write_irc_line(writer, &format!("KICK #{channel} {nick} :{reason}")).await;
                } else {
                    let _ = write_irc_line(writer, &format!("KICK #{channel} {nick}")).await;
                }
            }
            "invite" => {
                let (nick, chan) = parse_invite_command_args(
                    source_channel,
                    rest_raw,
                    self.command_context_channel(source_channel).as_deref(),
                );
                let Some(nick) = nick else {
                    self.emit_command_notice(source_channel, "Usage: /invite <nick> [#channel]")
                        .await;
                    return Ok(None);
                };
                let Some(channel) = chan else {
                    self.emit_command_notice(source_channel, "Usage: /invite <nick> [#channel]")
                        .await;
                    return Ok(None);
                };
                let _ = write_irc_line(writer, &format!("INVITE {nick} #{channel}")).await;
            }
            "whois" => {
                if rest_raw.trim().is_empty() {
                    self.emit_command_notice(source_channel, "Usage: /whois <nick>")
                        .await;
                } else {
                    let _ = write_irc_line(writer, &format!("WHOIS {}", rest_raw.trim())).await;
                }
            }
            "who" => {
                if rest_raw.trim().is_empty() {
                    if let Some(ch) = self.command_context_channel(source_channel) {
                        let _ = write_irc_line(writer, &format!("WHO #{ch}")).await;
                    } else {
                        self.emit_command_notice(source_channel, "Usage: /who <mask|#channel>")
                            .await;
                    }
                } else {
                    let _ = write_irc_line(writer, &format!("WHO {}", rest_raw.trim())).await;
                }
            }
            "away" => {
                if rest_raw.trim().is_empty() {
                    let _ = write_irc_line(writer, "AWAY").await;
                } else {
                    let _ = write_irc_line(writer, &format!("AWAY :{}", rest_raw.trim())).await;
                }
            }
            "quit" => {
                if rest_raw.trim().is_empty() {
                    let _ = write_irc_line(writer, "QUIT").await;
                } else {
                    let _ = write_irc_line(writer, &format!("QUIT :{}", rest_raw.trim())).await;
                }
                return Ok(Some(WorkerExit::Shutdown));
            }
            "pass" => {
                self.pass = if rest_raw.trim().is_empty() {
                    None
                } else {
                    Some(rest_raw.trim().to_owned())
                };
                let _ = write_irc_line(writer, "QUIT :Reconnecting to apply PASS").await;
                return Ok(Some(WorkerExit::Reconnect));
            }
            "user" => {
                let mut parts = rest_raw.splitn(2, char::is_whitespace);
                let Some(user) = parts.next().map(str::trim).filter(|s| !s.is_empty()) else {
                    self.emit_command_notice(source_channel, "Usage: /user <username> [realname]")
                        .await;
                    return Ok(None);
                };
                let realname = parts
                    .next()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(user);
                self.user = Some(user.to_owned());
                self.realname = Some(realname.to_owned());
                let _ = write_irc_line(writer, "QUIT :Reconnecting to apply USER").await;
                return Ok(Some(WorkerExit::Reconnect));
            }
            "nick" => {
                let Some(valid) = normalize_irc_nick(rest_raw.trim()) else {
                    self.emit_command_notice(source_channel, "Usage: /nick <nickname>")
                        .await;
                    return Ok(None);
                };
                self.nick = valid.clone();
                let _ = write_irc_line(writer, &format!("NICK {valid}")).await;
            }
            _ => {
                // Unknown slash command: pass it through as a raw IRC line.
                let _ = write_irc_line(writer, cmd_line).await;
            }
        }

        Ok(None)
    }
}

/// Returns true when `input` looks like a direct IRC protocol line (e.g.
/// `PRIVMSG #rust :hello`) that should be forwarded raw to the server.
pub fn is_raw_irc_protocol_line(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.starts_with('/') {
        return false;
    }

    let (verb, _rest) = trimmed
        .split_once(char::is_whitespace)
        .map(|(c, r)| (c, r.trim_start()))
        .unwrap_or((trimmed, ""));

    if verb.len() == 3 && verb.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    if !verb.bytes().all(|b| b.is_ascii_uppercase()) {
        return false;
    }

    matches!(
        verb,
        "PASS"
            | "NICK"
            | "USER"
            | "QUIT"
            | "PING"
            | "PONG"
            | "PRIVMSG"
            | "NOTICE"
            | "JOIN"
            | "PART"
            | "TOPIC"
            | "NAMES"
            | "LIST"
            | "MODE"
            | "KICK"
            | "INVITE"
            | "WHOIS"
            | "WHO"
            | "AWAY"
            | "CAP"
            | "AUTHENTICATE"
            | "MONITOR"
            | "ISON"
    )
}

fn parse_irc_system_notice(
    msg: &IrcMessage,
    key: &ServerKey,
    channels: &HashMap<String, Option<String>>,
    my_nick: &str,
) -> Option<SystemNotice> {
    let channel_hint = msg
        .params
        .iter()
        .find_map(|p| normalize_irc_target_channel(p));

    let channel_name = if let Some(ch) = channel_hint {
        ch
    } else if channels.contains_key(IRC_SERVER_CONTROL_CHANNEL) {
        IRC_SERVER_CONTROL_CHANNEL.to_owned()
    } else {
        channels
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| IRC_SERVER_CONTROL_CHANNEL.to_owned())
    };

    let text = match msg.command.as_str() {
        "NOTICE" => {
            let text = msg.trailing()?.trim();
            if is_ignored_irc_notice_line(text) {
                return None;
            }
            text.to_owned()
        }
        "JOIN" => {
            let nick = msg.nick().unwrap_or("someone");
            let joined = msg
                .params
                .first()
                .map(|c| c.trim().strip_prefix('#').unwrap_or(c.trim()))
                .unwrap_or(channel_name.as_str());
            format!("{nick} joined #{joined}")
        }
        "PART" => {
            let nick = msg.nick().unwrap_or("someone");
            let left = msg
                .params
                .first()
                .map(|c| c.trim().strip_prefix('#').unwrap_or(c.trim()))
                .unwrap_or(channel_name.as_str());
            if let Some(reason) = msg.trailing().filter(|s| !s.is_empty()) {
                format!("{nick} left #{left} ({reason})")
            } else {
                format!("{nick} left #{left}")
            }
        }
        "QUIT" => {
            let nick = msg.nick().unwrap_or("someone");
            if !nick.eq_ignore_ascii_case(my_nick) {
                return None;
            }
            if let Some(reason) = msg.trailing().filter(|s| !s.is_empty()) {
                format!("{nick} quit ({reason})")
            } else {
                format!("{nick} quit")
            }
        }
        "NICK" => {
            let old = msg.nick().unwrap_or("someone");
            let new_nick = msg.trailing().unwrap_or("unknown");
            if !old.eq_ignore_ascii_case(my_nick) && !new_nick.eq_ignore_ascii_case(my_nick) {
                return None;
            }
            format!("{old} is now known as {new_nick}")
        }
        "TOPIC" => {
            let nick = msg.nick().unwrap_or("someone");
            if let Some(topic) = msg.trailing() {
                format!("{nick} changed topic: {topic}")
            } else {
                return None;
            }
        }
        "MODE" => {
            let nick = msg.nick().unwrap_or("someone");
            if msg.params.len() >= 2 {
                let target = &msg.params[0];
                let mode = &msg.params[1];
                let mode_args = msg.params[2..].join(" ");
                if mode_args.is_empty() {
                    format!("{nick} set mode {mode} on {target}")
                } else {
                    format!("{nick} set mode {mode} {mode_args} on {target}")
                }
            } else {
                return None;
            }
        }
        "KICK" => {
            if msg.params.len() >= 2 {
                let chan = msg.params[0]
                    .trim()
                    .strip_prefix('#')
                    .unwrap_or(msg.params[0].trim());
                let target = &msg.params[1];
                let by = msg.nick().unwrap_or("someone");
                if let Some(reason) = msg.trailing().filter(|s| !s.is_empty()) {
                    format!("{by} kicked {target} from #{chan} ({reason})")
                } else {
                    format!("{by} kicked {target} from #{chan}")
                }
            } else {
                return None;
            }
        }
        "INVITE" => {
            if msg.params.len() >= 2 {
                let by = msg.nick().unwrap_or("someone");
                let target = &msg.params[0];
                let chan = msg.params[1].trim_start_matches('#');
                format!("{by} invited {target} to #{chan}")
            } else {
                return None;
            }
        }
        "CAP" => {
            let middle = msg
                .params
                .iter()
                .skip(1)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            let tail = msg.trailing().unwrap_or("").trim();
            if middle.is_empty() && tail.is_empty() {
                "CAP".to_owned()
            } else if tail.is_empty() {
                format!("CAP {middle}")
            } else if middle.is_empty() {
                format!("CAP {tail}")
            } else {
                format!("CAP {middle} :{tail}")
            }
        }
        cmd if cmd.chars().all(|c| c.is_ascii_digit()) => format_numeric_notice_text(msg, cmd)?,
        _ => return None,
    };

    Some(SystemNotice {
        channel: Some(ChannelId::irc(&key.host, key.port, key.tls, channel_name)),
        text,
        timestamp: Utc::now(),
    })
}

fn is_ignored_irc_notice_line(text: &str) -> bool {
    let t = text.trim().to_ascii_lowercase();
    t.starts_with("*** checking ident")
        || t.starts_with("*** looking up your hostname")
        || t.starts_with("*** couldn't look up your hostname")
        || t.starts_with("*** no ident response")
}

fn format_numeric_notice_text(msg: &IrcMessage, code: &str) -> Option<String> {
    let trailing = msg.trailing().map(str::trim).unwrap_or("");
    let mut params: Vec<&str> = msg.params.iter().map(String::as_str).collect();
    // Numeric replies usually target our nick in params[0]; hide it for readability.
    if !params.is_empty() {
        params.remove(0);
    }
    // The trailing parameter is also the last param in parsed output; avoid duplication.
    if !trailing.is_empty() && params.last().map(|p| p.trim()) == Some(trailing) {
        params.pop();
    }
    let middle = params.join(" ").trim().to_owned();

    match code {
        // Collapse startup burst into a few readable lines.
        "003" | "004" | "005" | "250" | "251" | "252" | "253" | "254" | "255" | "265" | "266" => {
            None
        }
        "375" => Some("[375] Message of the Day follows".to_owned()),
        // Show MOTD lines in a cleaned format.
        "372" => {
            let body = if !trailing.is_empty() {
                trailing
            } else {
                middle.as_str()
            };
            let body = body.trim();
            // Libera-style MOTD lines are usually prefixed with "- ".
            let body = body.strip_prefix('-').map(str::trim_start).unwrap_or(body);
            if body.is_empty() {
                None
            } else {
                Some(format!("[372] {body}"))
            }
        }
        "376" => Some("[376] End of MOTD".to_owned()),
        "002" => {
            let body = if !trailing.is_empty() {
                trailing
            } else {
                middle.as_str()
            };
            if body.is_empty() {
                None
            } else {
                Some(format!("[002] {body}"))
            }
        }
        _ => {
            if !middle.is_empty() && !trailing.is_empty() {
                Some(format!("[{code}] {middle} - {trailing}"))
            } else if !trailing.is_empty() {
                Some(format!("[{code}] {trailing}"))
            } else if !middle.is_empty() {
                Some(format!("[{code}] {middle}"))
            } else {
                None
            }
        }
    }
}

fn parse_irc_privmsg(msg: &IrcMessage, key: &ServerKey, my_nick: &str) -> Option<ChatMessage> {
    let target_raw = msg.params.first()?.trim();
    if target_raw.is_empty() {
        return None;
    }

    // Channel targets may be prefixed with status markers (@#chan, +#chan, ...).
    // If this is not a channel target, treat it as a direct-message target.
    let target_name = normalize_irc_target_channel(target_raw)
        .or_else(|| normalize_irc_nick(target_raw))
        .unwrap_or_else(|| target_raw.to_ascii_lowercase());
    let channel = ChannelId::irc(&key.host, key.port, key.tls, target_name);
    let raw_text = msg.trailing()?.to_owned();

    let (text, is_action) = if raw_text.starts_with("\x01ACTION ") && raw_text.ends_with('\x01') {
        (&raw_text[8..raw_text.len() - 1], true)
    } else {
        (raw_text.as_str(), false)
    };

    let login = msg.nick().unwrap_or("").to_owned();
    let display_name = msg.tags.get("display-name").unwrap_or(&login).to_owned();
    let color = msg
        .tags
        .get("color")
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let user_id = msg.tags.get("user-id").unwrap_or(&login).to_owned();
    let server_id = msg.tags.get("id").map(str::to_owned);

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

    let twitch_emotes =
        crust_core::format::parse_twitch_emotes_tag(msg.tags.get("emotes").unwrap_or(""));

    Some(ChatMessage {
        id: MessageId(IRC_MSG_ID.fetch_add(1, Ordering::Relaxed)),
        server_id,
        timestamp: Utc::now(),
        channel,
        sender: Sender {
            user_id: UserId(user_id),
            login: login.clone(),
            display_name,
            color,
            name_paint: None,
            badges,
        },
        raw_text: text.to_owned(),
        spans: smallvec::SmallVec::new(),
        twitch_emotes,
        flags: MessageFlags {
            is_action,
            is_highlighted: false,
            is_deleted: false,
            is_first_msg: false,
            is_pinned: false,
            is_self: login.eq_ignore_ascii_case(my_nick),
            is_mention: false,
            custom_reward_id: None,
            is_history: false,
        },
        reply: None,
        msg_kind: MsgKind::Chat,
    })
}

fn normalize_irc_channel_name(raw: &str) -> Option<String> {
    // Strip exactly one leading '#' - this preserves ##channels (e.g.
    // ##chat on Libera.Chat is stored internally as #chat and
    // reconstructed as ##chat when building IRC protocol lines).
    let trimmed = raw.trim();
    let name = trimmed
        .strip_prefix('#')
        .unwrap_or(trimmed)
        .to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    if name
        .chars()
        .any(|c| c.is_whitespace() || c == ',' || c == '\u{0007}' || c == ':')
    {
        return None;
    }
    Some(name)
}

fn normalize_irc_target_channel(raw: &str) -> Option<String> {
    let target = raw.trim().trim_start_matches(['@', '+', '%', '&', '~']);
    if !target.starts_with('#') {
        return None;
    }
    normalize_irc_channel_name(target)
}

fn normalize_irc_msg_target(raw: &str) -> String {
    if let Some(ch) = normalize_irc_target_channel(raw) {
        return format!("#{ch}");
    }
    if let Some(nick) = normalize_irc_nick(raw) {
        return nick;
    }
    raw.trim().to_owned()
}

fn strip_optional_irc_trailing_prefix(raw: &str) -> Option<&str> {
    let text = raw.trim_start();
    let text = text.strip_prefix(':').unwrap_or(text).trim_start();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn parse_part_command_args(
    source_channel: &str,
    rest: &str,
    context_channel: Option<&str>,
) -> (Option<String>, Option<String>) {
    let source_fallback = if source_channel == IRC_SERVER_CONTROL_CHANNEL {
        None
    } else {
        normalize_irc_channel_name(source_channel)
    };
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        return (
            context_channel
                .and_then(normalize_irc_channel_name)
                .or(source_fallback),
            None,
        );
    }

    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let first = parts.next().unwrap_or("").trim();
    let second = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    if first.starts_with('#') {
        (normalize_irc_channel_name(first), second)
    } else {
        (
            context_channel
                .and_then(normalize_irc_channel_name)
                .or(source_fallback),
            Some(trimmed.to_owned()),
        )
    }
}

fn parse_topic_command_args(
    rest: &str,
    context_channel: Option<String>,
) -> (Option<String>, Option<String>) {
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        return (
            context_channel.and_then(|c| normalize_irc_channel_name(&c)),
            None,
        );
    }

    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let first = parts.next().unwrap_or("").trim();
    let tail = parts.next().map(str::trim).filter(|s| !s.is_empty());

    if first.starts_with('#') {
        (normalize_irc_channel_name(first), tail.map(str::to_owned))
    } else {
        (
            context_channel.and_then(|c| normalize_irc_channel_name(&c)),
            Some(trimmed.to_owned()),
        )
    }
}

fn parse_kick_command_args(
    source_channel: &str,
    rest: &str,
    context_channel: Option<&str>,
) -> (Option<String>, Option<String>, Option<String>) {
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        return (None, None, None);
    }

    let mut parts = trimmed.split_whitespace();
    let first = parts.next().unwrap_or("");
    let second = parts.next().unwrap_or("");

    if first.starts_with('#') {
        if second.is_empty() {
            return (normalize_irc_channel_name(first), None, None);
        }
        let reason = parts.collect::<Vec<_>>().join(" ");
        (
            normalize_irc_channel_name(first),
            Some(second.to_owned()),
            if reason.is_empty() {
                None
            } else {
                Some(reason)
            },
        )
    } else {
        let source_fallback = if source_channel == IRC_SERVER_CONTROL_CHANNEL {
            None
        } else {
            normalize_irc_channel_name(source_channel)
        };
        let channel = context_channel
            .and_then(normalize_irc_channel_name)
            .or(source_fallback);
        let reason = if second.is_empty() {
            None
        } else {
            let rem = parts.collect::<Vec<_>>().join(" ");
            Some(if rem.is_empty() {
                second.to_owned()
            } else {
                format!("{second} {rem}")
            })
        };
        (channel, Some(first.to_owned()), reason)
    }
}

fn parse_invite_command_args(
    source_channel: &str,
    rest: &str,
    context_channel: Option<&str>,
) -> (Option<String>, Option<String>) {
    let mut parts = rest.split_whitespace();
    let nick = parts.next().map(str::to_owned);
    let channel = if let Some(raw) = parts.next() {
        normalize_irc_channel_name(raw)
    } else {
        let source_fallback = if source_channel == IRC_SERVER_CONTROL_CHANNEL {
            None
        } else {
            normalize_irc_channel_name(source_channel)
        };
        context_channel
            .and_then(normalize_irc_channel_name)
            .or(source_fallback)
    };
    (nick, channel)
}

async fn write_irc_line<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> std::io::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\r\n").await?;
    writer.flush().await
}

async fn write_join_line<W: AsyncWrite + Unpin>(
    writer: &mut W,
    channel: &str,
    key: Option<&str>,
) -> std::io::Result<()> {
    if let Some(key) = key.filter(|k| !k.trim().is_empty()) {
        write_irc_line(writer, &format!("JOIN #{channel} {key}")).await
    } else {
        write_irc_line(writer, &format!("JOIN #{channel}")).await
    }
}

fn random_guest_nick() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    format!("crust{}", (millis % 100_000) + 10_000)
}

fn normalize_irc_nick(raw: &str) -> Option<String> {
    let nick = raw.trim();
    if nick.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(nick.len().min(30));
    for ch in nick.chars() {
        if out.len() >= 30 {
            break;
        }
        let valid = ch.is_ascii_alphanumeric()
            || matches!(ch, '_' | '-' | '[' | ']' | '\\' | '^' | '{' | '}' | '|');
        if !valid {
            return None;
        }
        out.push(ch);
    }
    if out.is_empty() {
        return None;
    }
    let first = out.chars().next().unwrap_or('_');
    if !(first.is_ascii_alphabetic()
        || matches!(first, '_' | '[' | ']' | '\\' | '^' | '{' | '}' | '|'))
    {
        return None;
    }
    Some(out)
}

fn fallback_nick_after_collision(current: &str) -> String {
    let mut next = if current.starts_with("crust") {
        random_guest_nick()
    } else {
        format!("{current}_")
    };
    if next.len() > 30 {
        next.truncate(30);
    }
    normalize_irc_nick(&next).unwrap_or_else(random_guest_nick)
}

#[cfg(test)]
mod tests {
    use super::is_raw_irc_protocol_line;

    #[test]
    fn detects_raw_irc_privmsg_line() {
        assert!(is_raw_irc_protocol_line("PRIVMSG #rust :hello"));
    }

    #[test]
    fn rejects_plain_chat_text() {
        assert!(!is_raw_irc_protocol_line("hello everyone"));
        assert!(!is_raw_irc_protocol_line("join #rust"));
    }

    #[test]
    fn accepts_numeric_reply_style_line() {
        assert!(is_raw_irc_protocol_line("332 #rust :topic"));
    }
}

trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

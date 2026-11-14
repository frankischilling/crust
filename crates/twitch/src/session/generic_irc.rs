use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_native_tls::TlsConnector;
use tracing::{debug, error, info, warn};

use crust_core::model::{
    Badge, ChannelId, ChatMessage, MessageFlags, MessageId, MsgKind, Sender, SystemNotice, UserId,
    IRC_QUERY_CHANNEL_PREFIX, IRC_SERVER_CONTROL_CHANNEL,
};

use crate::irc::{parse_line, IrcMessage};

const BACKOFF_SECS: &[u64] = &[1, 2, 5, 10, 30];
const IRC_CMD_SIZE: usize = 64;
const IRC_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IRC_REGISTER_TIMEOUT: Duration = Duration::from_secs(10);
const IRC_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const IRC_IDLE_INTERVAL: Duration = Duration::from_secs(60);
const IRC_PONG_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_IRC_LINE_BYTES: usize = 64 * 1024;
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
    /// A direct-message tab opened on this server; no channel JOIN is required.
    PrivateConversationOpened {
        channel: ChannelId,
    },
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
    /// Synchronize channel bookkeeping after a worker reports a redirect.
    RedirectChannel { old: ChannelId, new: ChannelId },
    /// Completes after the write, returning the nickname confirmed by the server.
    SendMessage(ChannelId, String, oneshot::Sender<Result<String, String>>),
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
        receipt: oneshot::Sender<Result<String, String>>,
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

                    if servers.get(&server_key).is_some_and(|handle| handle.cmd_tx.is_closed()) {
                        servers.remove(&server_key);
                    }
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
                GenericIrcSessionCommand::RedirectChannel { old, new } => {
                    let (Some(old_key), Some(new_key), Some(old_target), Some(new_target)) = (
                        ServerKey::from_channel(&old), ServerKey::from_channel(&new),
                        old.irc_target(), new.irc_target(),
                    ) else { continue; };
                    if old_key == new_key {
                        if let Some(handle) = servers.get_mut(&old_key) {
                            if let Some(key) = handle.channels.remove(&old_target.channel.to_lowercase()) {
                                handle.channels.entry(new_target.channel.to_lowercase()).or_insert(key);
                            }
                        }
                    }
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
                GenericIrcSessionCommand::SendMessage(ch, text, receipt) => {
                    if !ch.is_irc() {
                        let _ = receipt.send(Err("Message was not sent. The IRC channel is invalid.".into()));
                        continue;
                    }
                    let Some(target) = ch.irc_target() else {
                        let _ = receipt.send(Err("Message was not sent. The IRC target is invalid.".into()));
                        continue;
                    };
                    let Some(key) = ServerKey::from_channel(&ch) else {
                        let _ = receipt.send(Err("Message was not sent. The IRC server is invalid.".into()));
                        continue;
                    };
                    if let Some(handle) = servers.get(&key) {
                        let _ = handle
                            .cmd_tx
                            .send(ServerCommand::Send {
                                channel: target.channel.to_lowercase(),
                                text,
                                receipt,
                            })
                            .await;
                    } else {
                        let _ = receipt.send(Err("Message was not sent. Connect to the IRC server first.".into()));
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
    confirmed_nick: Option<String>,
    pass: Option<String>,
    user: Option<String>,
    realname: Option<String>,
    /// NickServ username for auto-identification after 001.
    nickserv_user: String,
    /// NickServ password for auto-identification after 001.
    nickserv_pass: String,
    registered: bool,
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
            confirmed_nick: None,
            pass: None,
            user: None,
            realname: None,
            nickserv_user,
            nickserv_pass,
            registered: false,
        }
    }

    async fn emit(&self, evt: GenericIrcEvent) {
        let _ = self.event_tx.send(evt).await;
    }

    fn active_nick(&self) -> &str {
        self.confirmed_nick.as_deref().unwrap_or(&self.nick)
    }

    async fn handle_offline_command(&mut self, command: Option<ServerCommand>) -> bool {
        match command {
            None | Some(ServerCommand::Shutdown) => return true,
            Some(ServerCommand::Join { channel, key }) => {
                self.channels.insert(channel, key);
            }
            Some(ServerCommand::Leave(channel)) => {
                self.channels.remove(&channel);
                return self.channels.is_empty();
            }
            Some(ServerCommand::SetNick(nick)) => {
                if let Some(nick) = normalize_irc_nick(&nick) {
                    self.nick = nick;
                }
            }
            Some(ServerCommand::Send { receipt, .. }) => {
                let _ = receipt.send(Err("Message was not sent. IRC is reconnecting. Try again once connected.".into()));
            }
        }
        false
    }

    async fn wait_for_reconnect(&mut self, delay: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + delay;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return false,
                command = self.cmd_rx.recv() => {
                    if self.handle_offline_command(command).await {
                        return true;
                    }
                }
            }
        }
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
                    Some(ServerCommand::Send { receipt, .. }) => {
                        let _ = receipt.send(Err("Message was not sent. Join an IRC channel first.".into()));
                        continue;
                    }
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

            if self.registered {
                attempt = 0;
            }
            let delay = BACKOFF_SECS
                .get(attempt as usize)
                .copied()
                .unwrap_or(*BACKOFF_SECS.last().unwrap());
            if self.wait_for_reconnect(Duration::from_secs(delay)).await {
                self.emit(GenericIrcEvent::Disconnected { server: self.key.label() }).await;
                return;
            }
            attempt = attempt.saturating_add(1);
        }
    }

    async fn connect_once(&mut self) -> Result<WorkerExit, IrcSessionError> {
        self.registered = false;
        self.confirmed_nick = None;
        info!("Connecting to IRC server {}", self.key.label());
        let key = self.key.clone();
        let connecting = tokio::time::timeout(IRC_CONNECT_TIMEOUT, connect_irc_stream(&key));
        tokio::pin!(connecting);
        let stream = loop {
            tokio::select! {
                result = &mut connecting => break result.map_err(|_| irc_timeout("IRC connection timed out"))??,
                command = self.cmd_rx.recv() => {
                    if self.handle_offline_command(command).await {
                        return Ok(WorkerExit::Shutdown);
                    }
                }
            }
        };

        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        let mut line = Vec::<u8>::new();
        let registration_deadline = tokio::time::Instant::now() + IRC_REGISTER_TIMEOUT;
        let mut heartbeat_deadline = tokio::time::Instant::now() + IRC_IDLE_INTERVAL;
        let mut awaiting_pong = false;

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
            if is_irc_room(ch) {
                write_join_line(&mut write_half, ch, key.as_deref()).await?;
            }
        }

        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        None | Some(ServerCommand::Shutdown) => {
                            let _ = tokio::time::timeout(Duration::from_millis(250), write_irc_line(&mut write_half, "QUIT :Bye")).await;
                            return Ok(WorkerExit::Shutdown);
                        }
                        Some(ServerCommand::Join { channel, key }) => {
                            let previous = self.channels.insert(channel.clone(), key.clone());
                            if previous.is_none() || previous != Some(key.clone()) {
                                if is_irc_room(&channel) {
                                    write_join_line(&mut write_half, &channel, key.as_deref()).await?;
                                }
                            }
                        }
                        Some(ServerCommand::Leave(ch)) => {
                            if self.channels.remove(&ch).is_some() {
                                if is_irc_room(&ch) {
                                    write_irc_line(&mut write_half, &format!("PART #{ch}")).await?;
                                }
                                if self.channels.is_empty() {
                                    let _ = tokio::time::timeout(Duration::from_millis(250), write_irc_line(&mut write_half, "QUIT :No channels left")).await;
                                    return Ok(WorkerExit::Shutdown);
                                }
                            }
                        }
                        Some(ServerCommand::Send { channel, text, receipt }) => {
                            if let Some(exit) = self.send_with_receipt(&mut write_half, &channel, &text, receipt).await? {
                                return Ok(exit);
                            }
                        }
                        Some(ServerCommand::SetNick(nick)) => {
                            if let Some(valid) = normalize_irc_nick(&nick) {
                                self.nick = valid.clone();
                                write_irc_line(&mut write_half, &format!("NICK {valid}")).await?;
                            }
                        }
                    }
                }
                read_res = read_bounded_irc_line(&mut reader, &mut line) => {
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
                        heartbeat_deadline = tokio::time::Instant::now() + IRC_IDLE_INTERVAL;
                        awaiting_pong = false;
                        if msg.command == "PING" {
                            if let Some(token) = msg.trailing() {
                                write_irc_line(&mut write_half, &format!("PONG :{token}")).await?;
                            } else {
                                write_irc_line(&mut write_half, "PONG").await?;
                            }
                            continue;
                        }

                        if msg.command == "001" && !self.registered {
                            if let Some(nick) = msg.params.first().and_then(|nick| normalize_irc_nick(nick)) {
                                self.nick = nick.clone();
                                self.confirmed_nick = Some(nick);
                            }
                            self.registered = true;
                            self.emit(GenericIrcEvent::Connected {
                                server: self.key.label(),
                            }).await;

                            // Auto-identify with NickServ if credentials are configured.
                            if !self.nickserv_user.is_empty() && !self.nickserv_pass.is_empty() {
                                let identify_cmd = format!(
                                    "PRIVMSG NickServ :IDENTIFY {} {}",
                                    self.nickserv_user, self.nickserv_pass
                                );
                                write_irc_line(&mut write_half, &identify_cmd).await?;
                                info!("Sent NickServ IDENTIFY for {}", self.nickserv_user);
                                // Also change nick to the identified account name.
                                if let Some(valid) = normalize_irc_nick(&self.nickserv_user) {
                                    self.nick = valid.clone();
                                    write_irc_line(&mut write_half, &format!("NICK {valid}")).await?;
                                    info!("Auto nick-change to {valid} after NickServ IDENTIFY");
                                }
                            }

                            continue;
                        }

                        if msg.command == "NICK" {
                            if let (Some(old_nick), Some(new_nick)) = (
                                msg.nick(), msg.params.first().and_then(|nick| normalize_irc_nick(nick)),
                            ) {
                                if old_nick.eq_ignore_ascii_case(self.active_nick()) {
                                    self.nick = new_nick.clone();
                                    self.confirmed_nick = Some(new_nick);
                                } else {
                                    let old_channel = format!("{IRC_QUERY_CHANNEL_PREFIX}{}", old_nick.to_ascii_lowercase());
                                    let new_channel = format!("{IRC_QUERY_CHANNEL_PREFIX}{}", new_nick.to_ascii_lowercase());
                                    if old_channel != new_channel && self.channels.remove(&old_channel).is_some() {
                                        self.channels.entry(new_channel.clone()).or_insert(None);
                                        self.emit(GenericIrcEvent::ChannelRedirected {
                                            server_key: (self.key.host.clone(), self.key.port, self.key.tls),
                                            old_channel, new_channel,
                                        }).await;
                                    }
                                }
                            }
                        }

                        if msg.command == "433" {
                            self.nick = fallback_nick_after_collision(&self.nick);
                            write_irc_line(&mut write_half, &format!("NICK {}", self.nick)).await?;
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
                                    if let Some(key) = self.channels.remove(&old_ch) {
                                        self.channels.entry(new_ch.clone()).or_insert(key);
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
                            }
                            // Still let it fall through to parse_irc_system_notice so
                            // the user sees the notice text in chat.
                        }

                        if msg.command == "PRIVMSG" {
                            if let Some(chat) = parse_irc_privmsg(&msg, &self.key, self.active_nick()) {
                                if let Some(peer) = chat.channel.irc_query_nick() {
                                    self.open_private_conversation(peer).await;
                                }
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
                _ = tokio::time::sleep_until(registration_deadline), if !self.registered => {
                    return Err(irc_timeout("IRC registration timed out").into());
                }
                _ = tokio::time::sleep_until(heartbeat_deadline), if self.registered => {
                    if awaiting_pong {
                        return Err(irc_timeout("IRC heartbeat timed out").into());
                    }
                    write_irc_line(&mut write_half, "PING :crust").await?;
                    awaiting_pong = true;
                    heartbeat_deadline = tokio::time::Instant::now() + IRC_PONG_TIMEOUT;
                }
            }
        }
    }

    async fn open_private_conversation(&mut self, nick: &str) {
        let channel = ChannelId::irc_query(&self.key.host, self.key.port, self.key.tls, nick);
        let target = format!("{IRC_QUERY_CHANNEL_PREFIX}{}", nick.to_ascii_lowercase());
        if self.channels.insert(target, None).is_none() {
            self.emit(GenericIrcEvent::PrivateConversationOpened { channel }).await;
        }
    }

    fn command_context_channel(&self, source_channel: &str) -> Option<String> {
        if is_irc_room(source_channel) {
            return Some(source_channel.to_owned());
        }
        if source_channel.starts_with(IRC_QUERY_CHANNEL_PREFIX) {
            return None;
        }
        self.channels
            .keys()
            .find(|ch| is_irc_room(ch))
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

    async fn send_with_receipt<W: AsyncWrite + Unpin>(
        &mut self,
        writer: &mut W,
        source_channel: &str,
        text: &str,
        receipt: oneshot::Sender<Result<String, String>>,
    ) -> Result<Option<WorkerExit>, IrcSessionError> {
        let trimmed = text.trim_start();
        let command = trimmed.trim_start_matches('/').split_whitespace().next().unwrap_or_default();
        let is_chat = (!trimmed.starts_with('/') && !is_raw_irc_protocol_line(trimmed))
            || matches!(command.to_ascii_lowercase().as_str(), "me" | "msg" | "privmsg" | "notice");
        if !self.registered && is_chat {
            let _ = receipt.send(Err("Message was not sent. IRC is still connecting.".into()));
            return Ok(None);
        }
        match self.handle_outgoing_input(writer, source_channel, text).await {
            Ok(exit) => {
                let _ = receipt.send(Ok(self.active_nick().to_owned()));
                Ok(exit)
            }
            Err(error) => {
                let invalid = matches!(&error, IrcSessionError::Io(error) if error.kind() == std::io::ErrorKind::InvalidInput);
                let reason = if invalid {
                    format!("Message was not sent. {error}")
                } else {
                    format!("The IRC write did not complete: {error}. Check the channel before retrying.")
                };
                let _ = receipt.send(Err(reason));
                if invalid { Ok(None) } else { Err(error) }
            }
        }
    }

    async fn handle_outgoing_input<W: AsyncWrite + Unpin>(
        &mut self,
        writer: &mut W,
        source_channel: &str,
        text: &str,
    ) -> Result<Option<WorkerExit>, IrcSessionError> {
        if !crate::irc::types::is_single_irc_line(text) {
            return Err(irc_invalid_input("IRC messages cannot contain line breaks or null bytes.").into());
        }
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(irc_invalid_input("The message is empty.").into());
        }

        if !trimmed.starts_with('/') {
            if is_raw_irc_protocol_line(trimmed) {
                write_irc_line(writer, trimmed).await?;
                return Ok(None);
            }
            if source_channel != IRC_SERVER_CONTROL_CHANNEL
                && self.channels.contains_key(source_channel)
            {
                let target = irc_message_target(source_channel)?;
                write_irc_line(writer, &format!("PRIVMSG {target} :{trimmed}")).await?;
            } else {
                return Err(irc_invalid_input("Join a channel first (/join #channel) before sending messages.").into());
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
            "me" => {
                if source_channel == IRC_SERVER_CONTROL_CHANNEL || !self.channels.contains_key(source_channel) {
                    return Err(irc_invalid_input("Join a channel before sending an action.").into());
                }
                if rest_raw.is_empty() {
                    return Err(irc_invalid_input("Usage: /me <action>").into());
                }
                let target = irc_message_target(source_channel)?;
                write_irc_line(writer, &format!("PRIVMSG {target} :\u{0001}ACTION {rest_raw}\u{0001}")).await?;
            }
            "raw" => {
                if rest_raw.is_empty() {
                    self.emit_command_notice(source_channel, "Usage: /raw <line>")
                        .await;
                } else {
                    write_irc_line(writer, rest_raw).await?;
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
                write_irc_line(writer, &format!("PRIVMSG {target} :{body}")).await?;
                if !target.eq_ignore_ascii_case("nickserv") {
                    if let Some(nick) = normalize_irc_nick(&target).filter(|nick| nick.len() == target.len()) {
                        self.open_private_conversation(&nick).await;
                    }
                }

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
                            write_irc_line(writer, &format!("NICK {valid}")).await?;
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
                write_irc_line(writer, &format!("NOTICE {target} :{body}")).await?;
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
                if is_irc_room(&channel) {
                    write_join_line(writer, &channel, key.as_deref()).await?;
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
                if is_irc_room(&channel) {
                    if let Some(reason) = reason {
                        write_irc_line(writer, &format!("PART #{channel} :{reason}")).await?;
                    } else {
                        write_irc_line(writer, &format!("PART #{channel}")).await?;
                    }
                }
                self.channels.remove(&channel);
                if self.channels.is_empty() {
                    write_irc_line(writer, "QUIT :No channels left").await?;
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
                    write_irc_line(writer, &format!("TOPIC #{channel} :{topic}")).await?;
                } else {
                    write_irc_line(writer, &format!("TOPIC #{channel}")).await?;
                }
            }
            "names" => {
                if rest_raw.trim().is_empty() {
                    if let Some(ch) = self.command_context_channel(source_channel) {
                        write_irc_line(writer, &format!("NAMES #{ch}")).await?;
                    } else {
                        write_irc_line(writer, "NAMES").await?;
                    }
                } else {
                    write_irc_line(writer, &format!("NAMES {}", rest_raw.trim())).await?;
                }
            }
            "list" => {
                if rest_raw.trim().is_empty() {
                    write_irc_line(writer, "LIST").await?;
                } else {
                    write_irc_line(writer, &format!("LIST {}", rest_raw.trim())).await?;
                }
            }
            "mode" => {
                if rest_raw.trim().is_empty() {
                    if let Some(ch) = self.command_context_channel(source_channel) {
                        write_irc_line(writer, &format!("MODE #{ch}")).await?;
                    } else {
                        self.emit_command_notice(source_channel, "Usage: /mode <target> [modes]")
                            .await;
                    }
                } else {
                    write_irc_line(writer, &format!("MODE {}", rest_raw.trim())).await?;
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
                    write_irc_line(writer, &format!("KICK #{channel} {nick} :{reason}")).await?;
                } else {
                    write_irc_line(writer, &format!("KICK #{channel} {nick}")).await?;
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
                write_irc_line(writer, &format!("INVITE {nick} #{channel}")).await?;
            }
            "whois" => {
                if rest_raw.trim().is_empty() {
                    self.emit_command_notice(source_channel, "Usage: /whois <nick>")
                        .await;
                } else {
                    write_irc_line(writer, &format!("WHOIS {}", rest_raw.trim())).await?;
                }
            }
            "who" => {
                if rest_raw.trim().is_empty() {
                    if let Some(ch) = self.command_context_channel(source_channel) {
                        write_irc_line(writer, &format!("WHO #{ch}")).await?;
                    } else {
                        self.emit_command_notice(source_channel, "Usage: /who <mask|#channel>")
                            .await;
                    }
                } else {
                    write_irc_line(writer, &format!("WHO {}", rest_raw.trim())).await?;
                }
            }
            "away" => {
                if rest_raw.trim().is_empty() {
                    write_irc_line(writer, "AWAY").await?;
                } else {
                    write_irc_line(writer, &format!("AWAY :{}", rest_raw.trim())).await?;
                }
            }
            "quit" => {
                if rest_raw.trim().is_empty() {
                    write_irc_line(writer, "QUIT").await?;
                } else {
                    write_irc_line(writer, &format!("QUIT :{}", rest_raw.trim())).await?;
                }
                return Ok(Some(WorkerExit::Shutdown));
            }
            "pass" => {
                self.pass = if rest_raw.trim().is_empty() {
                    None
                } else {
                    Some(rest_raw.trim().to_owned())
                };
                write_irc_line(writer, "QUIT :Reconnecting to apply PASS").await?;
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
                write_irc_line(writer, "QUIT :Reconnecting to apply USER").await?;
                return Ok(Some(WorkerExit::Reconnect));
            }
            "nick" => {
                let Some(valid) = normalize_irc_nick(rest_raw.trim()) else {
                    self.emit_command_notice(source_channel, "Usage: /nick <nickname>")
                        .await;
                    return Ok(None);
                };
                self.nick = valid.clone();
                write_irc_line(writer, &format!("NICK {valid}")).await?;
            }
            _ => {
                // Unknown slash command: pass it through as a raw IRC line.
                write_irc_line(writer, cmd_line).await?;
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

    // Direct messages belong to the other participant, not to our own nickname.
    let channel = if let Some(target) = normalize_irc_target_channel(target_raw) {
        ChannelId::irc(&key.host, key.port, key.tls, target)
    } else {
        let sender = msg.nick()?;
        let peer = if target_raw.eq_ignore_ascii_case(my_nick) {
            sender
        } else if sender.eq_ignore_ascii_case(my_nick) {
            target_raw
        } else {
            return None;
        };
        let peer = normalize_irc_nick(peer).filter(|nick| nick.len() == peer.len())?;
        ChannelId::irc_query(&key.host, key.port, key.tls, peer)
    };
    let raw_text = msg.params.get(1)?;

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
            original_display_name: None,
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
            suppress_notification: false,
        },
        reply: None,
        msg_kind: MsgKind::Chat,
        shared: None,
    })
}

fn is_irc_room(channel: &str) -> bool {
    channel != IRC_SERVER_CONTROL_CHANNEL && !channel.starts_with(IRC_QUERY_CHANNEL_PREFIX)
}

fn irc_message_target(channel: &str) -> std::io::Result<String> {
    if let Some(peer) = channel.strip_prefix(IRC_QUERY_CHANNEL_PREFIX) {
        normalize_irc_nick(peer).filter(|nick| nick.len() == peer.len())
            .ok_or_else(|| irc_invalid_input("The private-message recipient is invalid."))
    } else {
        Ok(format!("#{channel}"))
    }
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

fn irc_timeout(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, message)
}

fn irc_invalid_input(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}

async fn connect_irc_stream(key: &ServerKey) -> Result<Box<dyn AsyncReadWrite>, IrcSessionError> {
    let tcp = TcpStream::connect((key.host.as_str(), key.port)).await?;
    tcp.set_nodelay(true)?;
    if key.tls {
        let tls = native_tls::TlsConnector::builder().build()?;
        let connector = TlsConnector::from(tls);
        Ok(Box::new(connector.connect(&key.host, tcp).await?))
    } else {
        Ok(Box::new(tcp))
    }
}

/// Preserve a partial line across select cancellation without retaining an
/// unlimited response from a peer that never sends a newline.
async fn read_bounded_irc_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> std::io::Result<usize> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(line.len());
        }
        let end = available.iter().position(|byte| *byte == b'\n');
        let count = end.map_or(available.len(), |position| position + 1);
        if count > MAX_IRC_LINE_BYTES.saturating_sub(line.len()) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "IRC line exceeds the 64 KiB limit"));
        }
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if end.is_some() {
            return Ok(line.len());
        }
    }
}

async fn write_irc_line<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> std::io::Result<()> {
    if !crate::irc::types::is_single_irc_line(line) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "IRC messages cannot contain line breaks or null bytes",
        ));
    }
    if line.len() > MAX_IRC_LINE_BYTES - 2 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "IRC line exceeds the 64 KiB limit"));
    }
    tokio::time::timeout(IRC_WRITE_TIMEOUT, async {
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\r\n").await?;
        writer.flush().await
    })
    .await
    .map_err(|_| irc_timeout("IRC write timed out"))?
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
    let (base, attempt) = current
        .rsplit_once('_')
        .and_then(|(base, suffix)| suffix.parse::<u32>().ok().map(|attempt| (base, attempt)))
        .unwrap_or((current, 0));
    let suffix = format!("_{}", attempt.wrapping_add(1));
    let mut next: String = base.chars().take(30 - suffix.len()).collect();
    next.push_str(&suffix);
    normalize_irc_nick(&next).unwrap_or_else(random_guest_nick)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(port: u16, tls: bool) -> (ServerWorker, mpsc::Sender<ServerCommand>, mpsc::Receiver<GenericIrcEvent>) {
        let (events, receiver) = mpsc::channel(16);
        let (commands, command_rx) = mpsc::channel(16);
        let worker = ServerWorker::new(
            ServerKey { host: "127.0.0.1".into(), port, tls },
            HashMap::from([("room".into(), None)]),
            events, command_rx, "alice".into(), String::new(), String::new(),
        );
        (worker, commands, receiver)
    }

    #[test]
    fn private_messages_route_to_the_other_participant_and_keep_rooms_separate() {
        let key = ServerKey { host: "example.com".into(), port: 6697, tls: true };
        let incoming = parse_line(":Bob!user@host PRIVMSG ALICE :hello").unwrap();
        let received = parse_irc_privmsg(&incoming, &key, "alice").unwrap();
        let query = ChannelId::irc_query("example.com", 6697, true, "bob");
        assert_eq!(received.channel, query);
        assert!(!received.flags.is_self);

        let outgoing = parse_line(":ALICE!user@host PRIVMSG Bob :hello back").unwrap();
        let echoed = parse_irc_privmsg(&outgoing, &key, "alice").unwrap();
        assert_eq!(echoed.channel, query);
        assert!(echoed.flags.is_self);

        let room = parse_line(":Bob!user@host PRIVMSG @#bob :channel message").unwrap();
        let room_message = parse_irc_privmsg(&room, &key, "alice").unwrap();
        assert_eq!(room_message.channel, ChannelId::irc("example.com", 6697, true, "bob"));
        assert_ne!(room_message.channel, query);

        let unrelated = parse_line(":Bob!user@host PRIVMSG carol :not ours").unwrap();
        assert!(parse_irc_privmsg(&unrelated, &key, "alice").is_none());
    }

    async fn read_test_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> String {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut line))
            .await.expect("IRC server did not receive a line").unwrap();
        line
    }

    #[tokio::test]
    async fn private_conversations_reply_without_joining_or_parting_a_room() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (mut worker, commands, mut events) = worker(port, false);
        worker.channels.insert(format!("{IRC_QUERY_CHANNEL_PREFIX}saved"), None);
        let task = tokio::spawn(async move { worker.connect_once().await });
        let (peer, _) = listener.accept().await.unwrap();
        let (read, mut write) = tokio::io::split(peer);
        let mut reader = BufReader::new(read);
        assert_eq!(read_test_line(&mut reader).await, "NICK alice\r\n");
        assert_eq!(read_test_line(&mut reader).await, "USER alice 0 * :alice\r\n");
        assert_eq!(read_test_line(&mut reader).await, "JOIN #room\r\n");
        write.write_all(b":server 001 alice :Welcome\r\nPING :ready\r\n:Bob!user@host PRIVMSG ALICE :hello\r\n").await.unwrap();
        assert_eq!(read_test_line(&mut reader).await, "PONG :ready\r\n");
        assert!(matches!(events.recv().await, Some(GenericIrcEvent::Connected { .. })));
        let query = ChannelId::irc_query("127.0.0.1", port, false, "bob");
        assert!(matches!(events.recv().await, Some(GenericIrcEvent::PrivateConversationOpened { channel }) if channel == query));
        assert!(matches!(events.recv().await, Some(GenericIrcEvent::ChatMessage(message)) if message.channel == query && message.raw_text == "hello"));

        for (text, wire) in [
            ("hello back", "PRIVMSG bob :hello back\r\n"),
            ("/me waves", "PRIVMSG bob :\u{0001}ACTION waves\u{0001}\r\n"),
        ] {
            let (receipt, result) = oneshot::channel();
            commands.send(ServerCommand::Send {
                channel: query.irc_target().unwrap().channel, text: text.into(), receipt,
            }).await.unwrap();
            assert_eq!(read_test_line(&mut reader).await, wire);
            assert_eq!(result.await.unwrap().unwrap(), "alice");
        }

        write.write_all(b":Bob!user@host NICK :Robert\r\n").await.unwrap();
        assert!(matches!(events.recv().await, Some(GenericIrcEvent::ChannelRedirected { old_channel, new_channel, .. })
            if old_channel == format!("{IRC_QUERY_CHANNEL_PREFIX}bob")
                && new_channel == format!("{IRC_QUERY_CHANNEL_PREFIX}robert")));
        let (receipt, result) = oneshot::channel();
        commands.send(ServerCommand::Send {
            channel: format!("{IRC_QUERY_CHANNEL_PREFIX}robert"), text: "new nickname".into(), receipt,
        }).await.unwrap();
        assert_eq!(read_test_line(&mut reader).await, "PRIVMSG robert :new nickname\r\n");
        assert!(result.await.unwrap().is_ok());

        for peer in ["saved", "robert"] {
            commands.send(ServerCommand::Leave(format!("{IRC_QUERY_CHANNEL_PREFIX}{peer}"))).await.unwrap();
        }
        let (receipt, result) = oneshot::channel();
        commands.send(ServerCommand::Send {
            channel: "room".into(), text: "PING :after-query-close".into(), receipt,
        }).await.unwrap();
        assert_eq!(read_test_line(&mut reader).await, "PING :after-query-close\r\n");
        assert!(result.await.unwrap().is_ok());
        commands.send(ServerCommand::Shutdown).await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(3), task).await.unwrap().unwrap(), Ok(WorkerExit::Shutdown)));
    }

    #[tokio::test]
    async fn private_msg_commands_open_a_query_only_after_a_successful_write() {
        let (mut worker, _commands, mut events) = worker(1, false);
        worker.registered = true;
        worker.confirmed_nick = Some("alice".into());
        let mut wire = Vec::new();
        let (receipt, result) = oneshot::channel();
        worker.send_with_receipt(&mut wire, "room", "/msg Bob :hello", receipt).await.unwrap();
        assert_eq!(wire, b"PRIVMSG Bob :hello\r\n");
        assert!(result.await.unwrap().is_ok());
        assert!(matches!(events.recv().await, Some(GenericIrcEvent::PrivateConversationOpened { channel })
            if channel.irc_query_nick() == Some("bob")));
        assert_eq!(worker.command_context_channel(&format!("{IRC_QUERY_CHANNEL_PREFIX}bob")), None);

        let (mut broken, peer) = tokio::io::duplex(1);
        drop(peer);
        let (receipt, result) = oneshot::channel();
        assert!(worker.send_with_receipt(&mut broken, "room", "/msg Carol hello", receipt).await.is_err());
        assert!(result.await.unwrap().is_err());
        assert!(!worker.channels.contains_key(&format!("{IRC_QUERY_CHANNEL_PREFIX}carol")));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn chat_receipt_waits_for_write_and_uses_confirmed_nick_for_actions() {
        let (mut worker, _commands, _events) = worker(1, false);
        worker.registered = true;
        worker.confirmed_nick = Some("alice_1".into());
        worker.nick = "unconfirmed-change".into();
        let (mut writer, reader) = tokio::io::duplex(1);
        let (receipt, mut received) = oneshot::channel();
        let task = tokio::spawn(async move {
            worker.send_with_receipt(&mut writer, "room", "/me waves", receipt).await
        });
        tokio::task::yield_now().await;
        assert!(matches!(received.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line)).await.unwrap().unwrap();
        assert_eq!(line, "PRIVMSG #room :\u{0001}ACTION waves\u{0001}\r\n");
        assert_eq!(received.await.unwrap().unwrap(), "alice_1");
        assert!(task.await.unwrap().unwrap().is_none());
    }

    #[tokio::test]
    async fn invalid_disconnected_and_failed_writes_complete_rejection_receipts() {
        let (mut worker, _commands, _events) = worker(1, false);
        let (receipt, received) = oneshot::channel();
        let mut sink = tokio::io::sink();
        worker.send_with_receipt(&mut sink, "room", "hello", receipt).await.unwrap();
        assert!(received.await.unwrap().unwrap_err().contains("connecting"));

        let (receipt, received) = oneshot::channel();
        worker.handle_offline_command(Some(ServerCommand::Send {
            channel: "room".into(), text: "hello".into(), receipt,
        })).await;
        assert!(received.await.unwrap().unwrap_err().contains("reconnecting"));

        worker.registered = true;
        for (channel, text) in [("room", "hello\r\nQUIT"), ("closed", "hello")] {
            let (receipt, received) = oneshot::channel();
            assert!(worker.send_with_receipt(&mut sink, channel, text, receipt).await.unwrap().is_none());
            assert!(received.await.unwrap().is_err());
        }

        let (mut writer, reader) = tokio::io::duplex(16);
        drop(reader);
        let (receipt, received) = oneshot::channel();
        assert!(worker.send_with_receipt(&mut writer, "room", "hello", receipt).await.is_err());
        assert!(received.await.unwrap().unwrap_err().contains("write did not complete"));
    }

    #[tokio::test]
    async fn unregistered_protocol_commands_can_still_authenticate() {
        let (mut worker, _commands, _events) = worker(1, false);
        let (mut writer, reader) = tokio::io::duplex(128);
        let (receipt, received) = oneshot::channel();
        worker.send_with_receipt(&mut writer, IRC_SERVER_CONTROL_CHANNEL, "PASS test-password", receipt).await.unwrap();
        assert!(received.await.unwrap().is_ok());
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line, "PASS test-password\r\n");
    }

    #[tokio::test]
    async fn closing_a_redirected_channel_allows_the_server_to_be_reopened() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (event_tx, mut events) = mpsc::channel(32);
        let (commands, command_rx) = mpsc::channel(16);
        let session = GenericIrcSession::new(event_tx, command_rx);
        let task = tokio::spawn(session.run());
        commands.send(GenericIrcSessionCommand::JoinChannel {
            channel: ChannelId::irc("127.0.0.1", port, false, "old"), key: None,
        }).await.unwrap();
        let (peer, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept()).await.unwrap().unwrap();
        let (read, mut write) = tokio::io::split(peer);
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        for _ in 0..3 {
            reader.read_line(&mut line).await.unwrap();
            line.clear();
        }
        write.write_all(b":server 001 guest :Welcome\r\n:server 470 guest #old #new :Forwarding\r\n").await.unwrap();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), events.recv()).await.unwrap().unwrap();
            if let GenericIrcEvent::ChannelRedirected { old_channel, new_channel, .. } = event {
                assert_eq!(old_channel, "old");
                assert_eq!(new_channel, "new");
                commands.send(GenericIrcSessionCommand::RedirectChannel {
                    old: ChannelId::irc("127.0.0.1", port, false, old_channel),
                    new: ChannelId::irc("127.0.0.1", port, false, new_channel),
                }).await.unwrap();
                break;
            }
        }
        commands.send(GenericIrcSessionCommand::LeaveChannel(ChannelId::irc("127.0.0.1", port, false, "new"))).await.unwrap();
        commands.send(GenericIrcSessionCommand::JoinChannel {
            channel: ChannelId::irc("127.0.0.1", port, false, "next"), key: None,
        }).await.unwrap();
        let (next, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept()).await.unwrap().unwrap();
        let mut next = BufReader::new(next);
        for expected in ["NICK ", "USER ", "JOIN #next"] {
            line.clear();
            tokio::time::timeout(Duration::from_secs(2), next.read_line(&mut line)).await.unwrap().unwrap();
            assert!(line.starts_with(expected), "expected {expected:?}, got {line:?}");
        }
        commands.send(GenericIrcSessionCommand::Disconnect).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn line_reader_preserves_partial_input_across_cancellation() {
        let (mut writer, input) = tokio::io::duplex(128);
        let mut reader = BufReader::new(input);
        let mut line = Vec::new();
        writer.write_all(b"PRIVMSG #room :hello ").await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(5), read_bounded_irc_line(&mut reader, &mut line)).await.is_err());
        assert_eq!(line, b"PRIVMSG #room :hello ");
        writer.write_all("🦀\r\nPING :next\r\n".as_bytes()).await.unwrap();
        assert_eq!(read_bounded_irc_line(&mut reader, &mut line).await.unwrap(), "PRIVMSG #room :hello 🦀\r\n".len());
        assert_eq!(line, "PRIVMSG #room :hello 🦀\r\n".as_bytes());
        line.clear();
        read_bounded_irc_line(&mut reader, &mut line).await.unwrap();
        assert_eq!(line, b"PING :next\r\n");
    }

    #[tokio::test]
    async fn oversized_lines_are_rejected_without_unbounded_retention() {
        let bytes = vec![b'x'; MAX_IRC_LINE_BYTES + 1];
        let mut reader = BufReader::new(bytes.as_slice());
        let mut line = Vec::new();
        let error = read_bounded_irc_line(&mut reader, &mut line).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(line.len() <= MAX_IRC_LINE_BYTES);
        let mut output = Vec::new();
        assert!(write_irc_line(&mut output, &"x".repeat(MAX_IRC_LINE_BYTES)).await.is_err());
        assert!(output.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_writes_have_a_deadline() {
        let (mut writer, _reader) = tokio::io::duplex(1);
        let start = tokio::time::Instant::now();
        let error = write_irc_line(&mut writer, "PING :blocked").await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(start.elapsed(), IRC_WRITE_TIMEOUT);
    }

    #[tokio::test]
    async fn shutdown_cancels_a_stalled_tls_handshake() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (mut worker, commands, _events) = worker(listener.local_addr().unwrap().port(), true);
        let task = tokio::spawn(async move { worker.connect_once().await });
        let (_peer, _) = listener.accept().await.unwrap();
        commands.send(ServerCommand::Shutdown).await.unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(1), task).await.unwrap().unwrap(), Ok(WorkerExit::Shutdown)));
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_wait_applies_commands_without_extending_the_deadline() {
        let (mut worker, commands, _events) = worker(6667, false);
        let producer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            commands.send(ServerCommand::Join { channel: "kept".into(), key: None }).await.unwrap();
            commands.send(ServerCommand::Leave("room".into())).await.unwrap();
            commands.send(ServerCommand::SetNick("bob".into())).await.unwrap();
            commands
        });
        let start = tokio::time::Instant::now();
        assert!(!worker.wait_for_reconnect(Duration::from_secs(30)).await);
        assert_eq!(start.elapsed(), Duration::from_secs(30));
        assert_eq!(worker.nick, "bob");
        assert!(worker.channels.contains_key("kept"));
        assert!(!worker.channels.contains_key("room"));
        let commands = producer.await.unwrap();
        commands.send(ServerCommand::Leave("kept".into())).await.unwrap();
        assert!(worker.wait_for_reconnect(Duration::from_secs(30)).await);
        assert_eq!(start.elapsed(), Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn silent_servers_time_out_before_and_after_registration() {
        for registered in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (mut worker, _commands, mut events) = worker(listener.local_addr().unwrap().port(), false);
            let task = tokio::spawn(async move { worker.connect_once().await });
            let (peer, _) = listener.accept().await.unwrap();
            let (read, mut write) = tokio::io::split(peer);
            let mut reader = BufReader::new(read);
            let mut line = String::new();
            for _ in 0..3 {
                reader.read_line(&mut line).await.unwrap();
                line.clear();
            }
            if registered {
                write.write_all(b":server 001 alice :Welcome\r\n").await.unwrap();
                assert!(matches!(events.recv().await, Some(GenericIrcEvent::Connected { .. })));
                tokio::time::advance(IRC_IDLE_INTERVAL).await;
                reader.read_line(&mut line).await.unwrap();
                assert_eq!(line, "PING :crust\r\n");
                tokio::time::advance(IRC_PONG_TIMEOUT).await;
            } else {
                tokio::time::advance(IRC_REGISTER_TIMEOUT).await;
            }
            let result = task.await.unwrap();
            assert!(matches!(result, Err(IrcSessionError::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut));
        }
    }

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
    fn collision_fallback_changes_full_length_nicknames() {
        let current = "a".repeat(30);
        let next = fallback_nick_after_collision(&current);
        assert_eq!(next.len(), 30);
        assert_ne!(next, current);
        assert!(normalize_irc_nick(&next).is_some());
        let retry = fallback_nick_after_collision(&next);
        assert_ne!(retry, next);
        assert_eq!(retry.len(), 30);
        assert_ne!(fallback_nick_after_collision("crust12345"), "crust12345");
    }

    #[test]
    fn accepts_numeric_reply_style_line() {
        assert!(is_raw_irc_protocol_line("332 #rust :topic"));
    }

    #[tokio::test]
    async fn rejects_injected_irc_lines_without_writing() {
        for line in [
            "PRIVMSG #room :hello\r\nQUIT",
            "PRIVMSG #room :hello\nQUIT",
            "PRIVMSG #room :hello\0world",
        ] {
            let mut bytes = Vec::new();
            let error = write_irc_line(&mut bytes, line).await.unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(bytes.is_empty());
        }
    }

    #[tokio::test]
    async fn writes_one_irc_terminator_and_preserves_unicode() {
        let mut bytes = Vec::new();
        write_irc_line(&mut bytes, "PRIVMSG #room :hello 🦀")
            .await
            .unwrap();
        assert_eq!(bytes, "PRIVMSG #room :hello 🦀\r\n".as_bytes());
    }

    #[test]
    fn ignores_privmsg_without_a_body() {
        let key = ServerKey {
            host: "example.com".into(),
            port: 6697,
            tls: true,
        };
        let message = parse_line(":alice!user@host PRIVMSG #room").unwrap();
        assert!(parse_irc_privmsg(&message, &key, "bob").is_none());
    }
}

trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

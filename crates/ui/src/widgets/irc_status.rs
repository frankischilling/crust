use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::Instant;

use chrono::{DateTime, Local, TimeZone, Utc};
use egui::{Color32, ComboBox, Context, RichText, Ui};

use crust_core::{
    events::AppEvent,
    model::{ChannelId, ChatMessage, MsgKind, IRC_SERVER_CONTROL_CHANNEL},
};

use crate::theme as t;

const IRC_EVENT_LOG_MAX: usize = 400;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct IrcServerKey {
    host: String,
    port: u16,
    tls: bool,
}

impl IrcServerKey {
    fn from_channel(channel: &ChannelId) -> Option<(Self, String)> {
        let target = channel.irc_target()?;
        Some((
            Self {
                host: target.host,
                port: target.port,
                tls: target.tls,
            },
            target.channel,
        ))
    }

    fn label(&self) -> String {
        if self.tls {
            format!("ircs://{}:{}", self.host, self.port)
        } else {
            format!("irc://{}:{}", self.host, self.port)
        }
    }
}

#[derive(Debug, Clone, Default)]
struct IrcChannelStatus {
    modes: Option<String>,
    topic: Option<String>,
    topic_setter: Option<String>,
    topic_set_unix: Option<i64>,
    creation_unix: Option<i64>,
    joined_with_key: bool,
    user_limit: Option<u32>,
    users_total: u32,
    ops_count: u32,
    halfops_count: u32,
    voiced_count: u32,
    normal_count: u32,
    your_privilege: Option<char>,
    ban_list_count: u32,
    except_list_count: u32,
    quiet_list_count: u32,
}

#[derive(Debug, Clone, Default)]
struct IrcWhoisEntry {
    nick: String,
    username: Option<String>,
    host: Option<String>,
    realname: Option<String>,
    account: Option<String>,
    server: Option<String>,
    idle_seconds: Option<u64>,
    signon_unix: Option<i64>,
    away_message: Option<String>,
    is_away: bool,
    is_operator: bool,
    shared_channels: Vec<String>,
}

#[derive(Debug, Clone)]
struct IrcServerStatus {
    configured_host: String,
    configured_port: u16,
    tls_enabled: bool,
    connected_server: Option<String>,
    network_name: Option<String>,
    remote_ip: Option<String>,
    connected: bool,
    connected_since: Option<Instant>,
    connected_since_wall: Option<DateTime<Utc>>,
    dropped_connections: u32,
    reconnect_attempts: u32,
    current_nick: Option<String>,
    username: Option<String>,
    realname: Option<String>,
    account_name: Option<String>,
    nickserv_identified: bool,
    user_modes: Option<String>,
    sasl_enabled: bool,
    sasl_mechanism: Option<String>,
    cap_ls: BTreeSet<String>,
    enabled_caps: BTreeSet<String>,
    last_numeric: Option<String>,
    motd_received: bool,
    monitor_list_size: Option<usize>,
    bytes_sent: u64,
    bytes_received: u64,
    messages_sent: u64,
    messages_received: u64,
    privmsg_sent: u64,
    privmsg_received: u64,
    notice_count: u64,
    join_part_count: u64,
    error_count: u64,
    ctcp_sent: u64,
    ctcp_received: u64,
    last_ping_ms: Option<f32>,
    avg_ping_ms: Option<f32>,
    max_ping_ms: Option<f32>,
    last_ping_unix: Option<i64>,
    last_pong_unix: Option<i64>,
    channels: HashMap<String, IrcChannelStatus>,
    whois: HashMap<String, IrcWhoisEntry>,
    selected_whois_nick: Option<String>,
    event_log: VecDeque<String>,
}

impl IrcServerStatus {
    fn new(key: &IrcServerKey) -> Self {
        Self {
            configured_host: key.host.clone(),
            configured_port: key.port,
            tls_enabled: key.tls,
            connected_server: None,
            network_name: None,
            remote_ip: None,
            connected: false,
            connected_since: None,
            connected_since_wall: None,
            dropped_connections: 0,
            reconnect_attempts: 0,
            current_nick: None,
            username: None,
            realname: None,
            account_name: None,
            nickserv_identified: false,
            user_modes: None,
            sasl_enabled: false,
            sasl_mechanism: None,
            cap_ls: BTreeSet::new(),
            enabled_caps: BTreeSet::new(),
            last_numeric: None,
            motd_received: false,
            monitor_list_size: None,
            bytes_sent: 0,
            bytes_received: 0,
            messages_sent: 0,
            messages_received: 0,
            privmsg_sent: 0,
            privmsg_received: 0,
            notice_count: 0,
            join_part_count: 0,
            error_count: 0,
            ctcp_sent: 0,
            ctcp_received: 0,
            last_ping_ms: None,
            avg_ping_ms: None,
            max_ping_ms: None,
            last_ping_unix: None,
            last_pong_unix: None,
            channels: HashMap::new(),
            whois: HashMap::new(),
            selected_whois_nick: None,
            event_log: VecDeque::new(),
        }
    }

    fn ensure_channel(&mut self, channel: &str) -> &mut IrcChannelStatus {
        self.channels.entry(channel.to_owned()).or_default()
    }

    fn push_event(&mut self, line: impl Into<String>) {
        let now = Local::now().format("%H:%M:%S");
        self.event_log.push_back(format!("{now} {}", line.into()));
        while self.event_log.len() > IRC_EVENT_LOG_MAX {
            self.event_log.pop_front();
        }
    }
}

#[derive(Default)]
pub struct IrcStatusPanel {
    servers: HashMap<IrcServerKey, IrcServerStatus>,
    selected_server: Option<IrcServerKey>,
}

impl IrcStatusPanel {
    pub fn on_event(&mut self, evt: &AppEvent) {
        match evt {
            AppEvent::ChannelJoined { channel } => {
                if let Some((key, ch_name)) = IrcServerKey::from_channel(channel) {
                    let server = self.ensure_server(&key);
                    server.ensure_channel(&ch_name);
                    if self.selected_server.is_none() {
                        self.selected_server = Some(key);
                    }
                }
            }
            AppEvent::ChannelParted { channel } => {
                if let Some((key, ch_name)) = IrcServerKey::from_channel(channel) {
                    if let Some(server) = self.servers.get_mut(&key) {
                        server.channels.remove(&ch_name);
                    }
                }
            }
            AppEvent::MessageReceived { channel, message } => {
                self.observe_message(channel, message);
            }
            AppEvent::Error { context, message } => {
                if context.starts_with("IRC") {
                    for server in self.servers.values_mut() {
                        server.error_count += 1;
                        server.push_event(format!("[error] {context}: {message}"));
                    }
                }
            }
            _ => {}
        }
    }

    pub fn note_outgoing(&mut self, channel: &ChannelId, text: &str) {
        let Some((key, ch_name)) = IrcServerKey::from_channel(channel) else {
            return;
        };
        let server = self.ensure_server(&key);
        server.messages_sent += 1;
        server.bytes_sent += text.len() as u64;

        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }

        if trimmed.starts_with('/') {
            let without = trimmed.trim_start_matches('/').trim_start();
            let cmd = without
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            Self::classify_outgoing_command(server, &ch_name, &cmd, without);
            server.push_event(format!("[out] /{without}"));
            return;
        }

        if looks_like_raw_irc_protocol_line(trimmed) {
            let cmd = trimmed
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            Self::classify_outgoing_command(server, &ch_name, &cmd, trimmed);
            server.push_event(format!("[out] {trimmed}"));
            return;
        }

        server.privmsg_sent += 1;
        if is_ctcp_body(trimmed) {
            server.ctcp_sent += 1;
        }
    }

    pub fn show(
        &mut self,
        ctx: &Context,
        mut open: bool,
        active_channel: Option<&ChannelId>,
    ) -> bool {
        if !open {
            return open;
        }

        if let Some(ch) = active_channel {
            if let Some((key, _)) = IrcServerKey::from_channel(ch) {
                self.selected_server = Some(key);
            }
        }

        let mut keys: Vec<IrcServerKey> = self.servers.keys().cloned().collect();
        keys.sort_by_key(|k| k.label());
        if self.selected_server.is_none() {
            self.selected_server = keys.first().cloned();
        }
        if let Some(sel) = self.selected_server.clone() {
            if !self.servers.contains_key(&sel) {
                self.selected_server = keys.first().cloned();
            }
        }

        egui::Window::new("IRC Status")
            .open(&mut open)
            .default_size(egui::vec2(760.0, 760.0))
            .default_pos(egui::pos2(90.0, 60.0))
            .min_width(520.0)
            .show(ctx, |ui| {
                if keys.is_empty() {
                    ui.label(
                        RichText::new(
                            "No IRC servers tracked yet. Join an IRC server/channel first.",
                        )
                        .color(t::TEXT_MUTED),
                    );
                    return;
                }

                ui.horizontal(|ui| {
                    ui.label(RichText::new("Server").color(t::TEXT_SECONDARY));
                    ComboBox::from_id_salt("irc_status_server_select")
                        .selected_text(
                            self.selected_server
                                .as_ref()
                                .map(IrcServerKey::label)
                                .unwrap_or_else(|| "Select server".to_owned()),
                        )
                        .show_ui(ui, |ui| {
                            for key in &keys {
                                ui.selectable_value(
                                    &mut self.selected_server,
                                    Some(key.clone()),
                                    key.label(),
                                );
                            }
                        });
                });
                ui.separator();

                let Some(sel) = self.selected_server.clone() else {
                    return;
                };
                let Some(server) = self.servers.get_mut(&sel) else {
                    return;
                };

                ui.horizontal_wrapped(|ui| {
                    status_chip(
                        ui,
                        if server.connected {
                            "Connected"
                        } else {
                            "Disconnected"
                        },
                        if server.connected { t::GREEN } else { t::RED },
                    );
                    status_chip(
                        ui,
                        &format!("TLS: {}", yes_no(server.tls_enabled)),
                        t::TEXT_SECONDARY,
                    );
                    status_chip(
                        ui,
                        &format!("Nick: {}", opt_text(server.current_nick.clone())),
                        t::TEXT_SECONDARY,
                    );
                    status_chip(
                        ui,
                        &format!(
                            "Channels: {}",
                            server
                                .channels
                                .keys()
                                .filter(|c| c.as_str() != IRC_SERVER_CONTROL_CHANNEL)
                                .count()
                        ),
                        t::TEXT_SECONDARY,
                    );
                    status_chip(
                        ui,
                        &format!("WHOIS cache: {}", server.whois.len()),
                        t::TEXT_SECONDARY,
                    );
                });
                ui.add_space(6.0);

                egui::ScrollArea::vertical()
                    .id_salt(egui::Id::new("irc_status_scroll").with(sel.label()))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        Self::section_frame(
                            ui,
                            "1. Connection Overview",
                            "Network, identity, and keepalive state",
                            |ui| Self::show_connection_overview(ui, server),
                        );
                        ui.add_space(8.0);
                        Self::section_frame(
                            ui,
                            "2. Channel State",
                            "Selected channel metadata and membership snapshot",
                            |ui| Self::show_channel_state(ui, server, active_channel, &sel),
                        );
                        ui.add_space(8.0);
                        Self::section_frame(
                            ui,
                            "3. IRC Protocol Status",
                            "Capabilities, numerics, and protocol-level state",
                            |ui| Self::show_protocol_status(ui, server),
                        );
                        ui.add_space(8.0);
                        Self::section_frame(
                            ui,
                            "4. Traffic / Performance",
                            "Counters and rate metrics",
                            |ui| Self::show_traffic_panel(ui, server),
                        );
                        ui.add_space(8.0);
                        Self::section_frame(
                            ui,
                            "5. Security",
                            "TLS/SASL/identity security posture",
                            |ui| Self::show_security_panel(ui, server),
                        );
                        ui.add_space(8.0);
                        Self::section_frame(
                            ui,
                            "6. Status / Event Log",
                            "Live categorized IRC lifecycle and protocol events",
                            |ui| Self::show_event_log(ui, server, &sel),
                        );
                        ui.add_space(8.0);
                        Self::section_frame(
                            ui,
                            "7. Per-User Inspect",
                            "WHOIS cache inspector",
                            |ui| Self::show_user_inspect(ui, server, &sel),
                        );
                    });
            });

        open
    }

    fn section_frame(ui: &mut Ui, title: &str, subtitle: &str, add_contents: impl FnOnce(&mut Ui)) {
        egui::Frame::new()
            .fill(t::BG_SURFACE)
            .stroke(egui::Stroke::new(1.0, t::BORDER_SUBTLE))
            .corner_radius(t::RADIUS)
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(title)
                        .font(t::body())
                        .strong()
                        .color(t::TEXT_PRIMARY),
                );
                ui.label(
                    RichText::new(subtitle)
                        .font(t::small())
                        .color(t::TEXT_MUTED),
                );
                ui.add_space(6.0);
                add_contents(ui);
            });
    }

    fn ensure_server(&mut self, key: &IrcServerKey) -> &mut IrcServerStatus {
        self.servers
            .entry(key.clone())
            .or_insert_with(|| IrcServerStatus::new(key))
    }

    fn classify_outgoing_command(
        server: &mut IrcServerStatus,
        source_channel: &str,
        command_upper: &str,
        full_line: &str,
    ) {
        match command_upper {
            "PRIVMSG" | "MSG" => {
                server.privmsg_sent += 1;
                if is_ctcp_body(full_line) {
                    server.ctcp_sent += 1;
                }
            }
            "NOTICE" => server.notice_count += 1,
            "JOIN" | "PART" => server.join_part_count += 1,
            "PING" => server.last_ping_unix = Some(Utc::now().timestamp()),
            "PONG" => server.last_pong_unix = Some(Utc::now().timestamp()),
            "NICK" => {
                if let Some(new_nick) = full_line.split_whitespace().nth(1) {
                    server.current_nick = Some(new_nick.trim_start_matches(':').to_owned());
                }
            }
            "USER" => {
                let mut parts = full_line.split_whitespace();
                let _ = parts.next();
                if let Some(user) = parts.next() {
                    server.username = Some(user.to_owned());
                }
            }
            "PASS" => {}
            "SASL" => server.sasl_enabled = true,
            _ => {}
        }

        if command_upper == "JOIN" {
            let key = full_line.split_whitespace().nth(2);
            if let Some(ch) =
                normalize_channel_token(full_line.split_whitespace().nth(1).unwrap_or(""))
            {
                let status = server.ensure_channel(&ch);
                status.joined_with_key = key.is_some();
            } else {
                let status = server.ensure_channel(source_channel);
                status.joined_with_key = key.is_some();
            }
        }
    }

    fn observe_message(&mut self, channel: &ChannelId, message: &ChatMessage) {
        let Some((key, ch_name)) = IrcServerKey::from_channel(channel) else {
            return;
        };
        let server = self.ensure_server(&key);

        server.messages_received += 1;
        server.bytes_received += message.raw_text.len() as u64;

        if message.msg_kind == MsgKind::Chat {
            server.privmsg_received += 1;
            if is_ctcp_body(&message.raw_text) {
                server.ctcp_received += 1;
            }
            if !message.sender.login.is_empty() {
                let ch = server.ensure_channel(&ch_name);
                ch.users_total = ch.users_total.max(1);
            }
            return;
        }

        if message.msg_kind != MsgKind::SystemInfo {
            return;
        }

        server.notice_count += 1;
        let text = message.raw_text.trim();
        if text.is_empty() {
            return;
        }
        Self::parse_system_notice(server, &ch_name, text);
    }

    fn parse_system_notice(server: &mut IrcServerStatus, source_channel: &str, text: &str) {
        if let Some(s) = text.strip_prefix("IRC connected: ") {
            server.connected = true;
            server.connected_since = Some(Instant::now());
            server.connected_since_wall = Some(Utc::now());
            server.push_event(format!("[conn] connected to {}", s.trim()));
            return;
        }
        if let Some(s) = text.strip_prefix("IRC disconnected: ") {
            server.connected = false;
            server.connected_since = None;
            server.dropped_connections += 1;
            server.push_event(format!("[conn] disconnected from {}", s.trim()));
            return;
        }
        if text.starts_with("IRC reconnecting (") {
            if let Some(attempt) = text
                .split("attempt")
                .nth(1)
                .and_then(|v| v.trim().parse::<u32>().ok())
            {
                server.reconnect_attempts = attempt;
            }
            server.push_event(format!("[conn] {}", text));
            return;
        }
        if let Some(new_nick) = text
            .split(" is now known as ")
            .nth(1)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            server.current_nick = Some(new_nick.to_owned());
            server.push_event(format!("[nick] {}", text));
            return;
        }
        if let Some(caps) = text.strip_prefix("CAP ") {
            Self::apply_cap_status(server, caps);
            server.push_event(format!("[cap] {text}"));
            return;
        }
        if let Some((actor, mode, target, args)) = parse_mode_setter_line(text) {
            if target.starts_with('#') {
                if let Some(channel_name) = normalize_channel_token(&target) {
                    let st = server.ensure_channel(&channel_name);
                    st.modes = Some(mode.clone());
                    if mode.contains('k') {
                        st.joined_with_key = true;
                    }
                    if mode.contains('l') {
                        st.user_limit = args
                            .split_whitespace()
                            .rev()
                            .find_map(|v| v.parse::<u32>().ok());
                    }
                    server.push_event(format!("[mode] {actor} set {mode} on #{channel_name}"));
                }
            } else {
                server.user_modes = Some(mode.clone());
                if target.eq_ignore_ascii_case(server.current_nick.as_deref().unwrap_or("")) {
                    server.current_nick = Some(target.clone());
                }
                server.push_event(format!("[mode] {actor} set user mode {mode}"));
            }
            return;
        }

        if let Some((code, body)) = parse_numeric_line(text) {
            server.last_numeric = Some(code.to_owned());
            Self::parse_numeric_notice(server, source_channel, code, body);
            return;
        }

        if text.starts_with("***") {
            server.push_event(format!("[notice] {text}"));
            return;
        }
        if text.starts_with("This nickname is registered")
            || text.to_ascii_lowercase().contains("nickserv")
        {
            server.nickserv_identified = text.to_ascii_lowercase().contains("identified");
            server.push_event(format!("[auth] {text}"));
            return;
        }

        if let Some(host_line) = text.strip_prefix("Your host is ") {
            parse_host_line(server, host_line);
            server.push_event(format!("[server] {text}"));
            return;
        }

        server.push_event(format!("[info] {text}"));
    }

    fn parse_numeric_notice(
        server: &mut IrcServerStatus,
        source_channel: &str,
        code: &str,
        body: &str,
    ) {
        match code {
            "001" => {
                if let Some(nick) = body
                    .split_whitespace()
                    .next()
                    .filter(|s| !s.starts_with('['))
                {
                    server.current_nick = Some(nick.trim_start_matches(':').to_owned());
                }
                server.push_event(format!("[{code}] {}", body.trim()));
            }
            "002" => {
                parse_host_line(server, body);
                server.push_event(format!("[{code}] {}", body.trim()));
            }
            "005" => {
                if let Some(net) = extract_irc_feature_value(body, "NETWORK") {
                    server.network_name = Some(net);
                }
                if let Some(mon) = extract_irc_feature_value(body, "MONITOR") {
                    server.monitor_list_size = mon.parse::<usize>().ok();
                }
                server.push_event(format!("[{code}] {}", body.trim()));
            }
            "221" => {
                let mode = body
                    .split_whitespace()
                    .find(|p| p.starts_with('+') || p.starts_with('-'))
                    .map(str::to_owned);
                if mode.is_some() {
                    server.user_modes = mode;
                }
                server.push_event(format!("[{code}] {}", body.trim()));
            }
            "375" => {
                server.motd_received = true;
                server.push_event("[motd] begin".to_owned());
            }
            "372" => {
                server.push_event(format!("[motd] {}", body.trim()));
            }
            "376" => {
                server.push_event("[motd] end".to_owned());
            }
            "324" => {
                let (middle, _) = split_middle_and_trailing(body);
                let mut parts = middle.split_whitespace();
                let chan = parts.next().and_then(normalize_channel_token);
                let modes = parts.next().map(str::to_owned);
                if let (Some(ch), Some(m)) = (chan, modes) {
                    server.ensure_channel(&ch).modes = Some(m);
                }
                server.push_event(format!("[{code}] {}", body.trim()));
            }
            "329" => {
                let (middle, _) = split_middle_and_trailing(body);
                let mut parts = middle.split_whitespace();
                let chan = parts.next().and_then(normalize_channel_token);
                let ts = parts.next().and_then(|v| v.parse::<i64>().ok());
                if let (Some(ch), Some(ts)) = (chan, ts) {
                    server.ensure_channel(&ch).creation_unix = Some(ts);
                }
                server.push_event(format!("[{code}] {}", body.trim()));
            }
            "332" => {
                let (middle, trailing) = split_middle_and_trailing(body);
                let ch = middle
                    .split_whitespace()
                    .find_map(normalize_channel_token)
                    .or_else(|| normalize_channel_token(source_channel));
                if let Some(ch) = ch {
                    if let Some(topic) = trailing {
                        server.ensure_channel(&ch).topic = Some(topic.to_owned());
                    }
                }
                server.push_event(format!("[{code}] {}", body.trim()));
            }
            "333" => {
                let (middle, _) = split_middle_and_trailing(body);
                let mut parts = middle.split_whitespace();
                let ch = parts.next().and_then(normalize_channel_token);
                let setter = parts.next().map(str::to_owned);
                let ts = parts.next().and_then(|v| v.parse::<i64>().ok());
                if let Some(ch) = ch {
                    let st = server.ensure_channel(&ch);
                    st.topic_setter = setter;
                    st.topic_set_unix = ts;
                }
                server.push_event(format!("[{code}] {}", body.trim()));
            }
            "353" => {
                let (middle, trailing) = split_middle_and_trailing(body);
                let chan = middle
                    .split_whitespace()
                    .find_map(normalize_channel_token)
                    .or_else(|| normalize_channel_token(source_channel));
                if let Some(ch_name) = chan {
                    let names_blob = trailing.unwrap_or("");
                    Self::apply_names_list(server, &ch_name, names_blob);
                }
                server.push_event(format!("[{code}] {}", body.trim()));
            }
            "367" => {
                let (middle, _) = split_middle_and_trailing(body);
                if let Some(ch) = middle.split_whitespace().find_map(normalize_channel_token) {
                    server.ensure_channel(&ch).ban_list_count += 1;
                }
            }
            "348" | "728" => {
                let (middle, _) = split_middle_and_trailing(body);
                if let Some(ch) = middle.split_whitespace().find_map(normalize_channel_token) {
                    server.ensure_channel(&ch).quiet_list_count += 1;
                }
            }
            "346" => {
                let (middle, _) = split_middle_and_trailing(body);
                if let Some(ch) = middle.split_whitespace().find_map(normalize_channel_token) {
                    server.ensure_channel(&ch).except_list_count += 1;
                }
            }
            "900" | "903" => {
                server.nickserv_identified = true;
                if let Some(acc) = body
                    .split_whitespace()
                    .find(|s| s.contains('!'))
                    .map(|s| s.split('!').next().unwrap_or(s))
                {
                    server.account_name = Some(acc.to_owned());
                }
                server.sasl_enabled = true;
                server.push_event(format!("[auth] [{code}] {}", body.trim()));
            }
            "904" | "905" | "906" | "907" => {
                server.sasl_enabled = true;
                server.push_event(format!("[auth] [{code}] {}", body.trim()));
            }
            "301" => {
                // WHOIS away line
                let (middle, trailing) = split_middle_and_trailing(body);
                let nick = middle
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_start_matches('#')
                    .to_owned();
                if !nick.is_empty() {
                    let entry = server
                        .whois
                        .entry(nick.clone())
                        .or_insert_with(|| IrcWhoisEntry {
                            nick: nick.clone(),
                            ..IrcWhoisEntry::default()
                        });
                    entry.is_away = true;
                    entry.away_message = trailing.map(str::to_owned);
                }
            }
            "311" => {
                let (middle, trailing) = split_middle_and_trailing(body);
                let mut p = middle.split_whitespace();
                let nick = p.next().unwrap_or("").to_owned();
                let user = p.next().map(str::to_owned);
                let host = p.next().map(str::to_owned);
                if !nick.is_empty() {
                    let entry = server
                        .whois
                        .entry(nick.clone())
                        .or_insert_with(|| IrcWhoisEntry {
                            nick: nick.clone(),
                            ..IrcWhoisEntry::default()
                        });
                    entry.username = user;
                    entry.host = host;
                    entry.realname = trailing.map(str::to_owned);
                    if server.selected_whois_nick.is_none() {
                        server.selected_whois_nick = Some(nick);
                    }
                }
            }
            "312" => {
                let (middle, trailing) = split_middle_and_trailing(body);
                let mut p = middle.split_whitespace();
                let nick = p.next().unwrap_or("").to_owned();
                let server_name = p.next().map(str::to_owned);
                if !nick.is_empty() {
                    let entry = server
                        .whois
                        .entry(nick.clone())
                        .or_insert_with(|| IrcWhoisEntry {
                            nick,
                            ..IrcWhoisEntry::default()
                        });
                    entry.server = server_name.or_else(|| trailing.map(str::to_owned));
                }
            }
            "313" => {
                let (middle, _) = split_middle_and_trailing(body);
                let nick = middle.split_whitespace().next().unwrap_or("").to_owned();
                if !nick.is_empty() {
                    let entry = server
                        .whois
                        .entry(nick.clone())
                        .or_insert_with(|| IrcWhoisEntry {
                            nick,
                            ..IrcWhoisEntry::default()
                        });
                    entry.is_operator = true;
                }
            }
            "317" => {
                let (middle, _) = split_middle_and_trailing(body);
                let mut p = middle.split_whitespace();
                let nick = p.next().unwrap_or("").to_owned();
                let idle = p.next().and_then(|v| v.parse::<u64>().ok());
                let signon = p.next().and_then(|v| v.parse::<i64>().ok());
                if !nick.is_empty() {
                    let entry = server
                        .whois
                        .entry(nick.clone())
                        .or_insert_with(|| IrcWhoisEntry {
                            nick,
                            ..IrcWhoisEntry::default()
                        });
                    entry.idle_seconds = idle;
                    entry.signon_unix = signon;
                }
            }
            "319" => {
                let (middle, trailing) = split_middle_and_trailing(body);
                let nick = middle.split_whitespace().next().unwrap_or("").to_owned();
                if !nick.is_empty() {
                    let chans = trailing
                        .unwrap_or("")
                        .split_whitespace()
                        .map(|v| v.trim_start_matches(['@', '+', '%', '&', '~']))
                        .filter(|v| v.starts_with('#'))
                        .map(str::to_owned)
                        .collect::<Vec<_>>();
                    let entry = server
                        .whois
                        .entry(nick.clone())
                        .or_insert_with(|| IrcWhoisEntry {
                            nick,
                            ..IrcWhoisEntry::default()
                        });
                    entry.shared_channels = chans;
                }
            }
            "330" => {
                let (middle, _) = split_middle_and_trailing(body);
                let mut p = middle.split_whitespace();
                let nick = p.next().unwrap_or("").to_owned();
                let account = p.next().map(str::to_owned);
                if !nick.is_empty() {
                    let entry = server
                        .whois
                        .entry(nick.clone())
                        .or_insert_with(|| IrcWhoisEntry {
                            nick,
                            ..IrcWhoisEntry::default()
                        });
                    entry.account = account;
                }
            }
            "671" => {
                let (middle, _) = split_middle_and_trailing(body);
                let nick = middle.split_whitespace().next().unwrap_or("").to_owned();
                if !nick.is_empty() {
                    let entry = server
                        .whois
                        .entry(nick.clone())
                        .or_insert_with(|| IrcWhoisEntry {
                            nick,
                            ..IrcWhoisEntry::default()
                        });
                    entry.server = entry
                        .server
                        .clone()
                        .or_else(|| Some("secure connection".to_owned()));
                }
            }
            "318" => {
                server.push_event(format!("[whois] {}", body.trim()));
            }
            _ => {
                server.push_event(format!("[{code}] {}", body.trim()));
            }
        }
    }

    fn apply_names_list(server: &mut IrcServerStatus, channel: &str, names_blob: &str) {
        let mut ops = 0u32;
        let mut halfops = 0u32;
        let mut voice = 0u32;
        let mut normal = 0u32;
        let mut mine = None;
        let current_nick = server
            .current_nick
            .clone()
            .unwrap_or_default()
            .to_lowercase();

        for token in names_blob.split_whitespace() {
            let clean = token.trim();
            if clean.is_empty() {
                continue;
            }

            let (prefix, nick) = if let Some(first) = clean.chars().next() {
                if "~&@%+".contains(first) {
                    (Some(first), clean[first.len_utf8()..].to_owned())
                } else {
                    (None, clean.to_owned())
                }
            } else {
                (None, String::new())
            };
            if nick.is_empty() {
                continue;
            }

            match prefix {
                Some('~') | Some('&') | Some('@') => ops += 1,
                Some('%') => halfops += 1,
                Some('+') => voice += 1,
                _ => normal += 1,
            }

            if !current_nick.is_empty() && nick.eq_ignore_ascii_case(&current_nick) {
                mine = prefix;
            }
        }

        let st = server.ensure_channel(channel);
        st.ops_count = ops;
        st.halfops_count = halfops;
        st.voiced_count = voice;
        st.normal_count = normal;
        st.users_total = ops + halfops + voice + normal;
        st.your_privilege = mine;
    }

    fn apply_cap_status(server: &mut IrcServerStatus, caps: &str) {
        let c = caps.trim();
        if c.is_empty() {
            return;
        }
        let upper = c.to_ascii_uppercase();
        if upper.contains(" LS") {
            if let Some(list) = c.split(':').nth(1) {
                for cap in list.split_whitespace().map(|v| v.trim()) {
                    if !cap.is_empty() {
                        server.cap_ls.insert(cap.to_owned());
                    }
                }
            }
        }
        if upper.contains(" ACK") {
            if let Some(list) = c.split(':').nth(1).or_else(|| c.split_whitespace().last()) {
                for cap in list.split_whitespace().map(|v| v.trim()) {
                    if !cap.is_empty() {
                        server.enabled_caps.insert(cap.to_owned());
                    }
                }
            }
        }
        if upper.contains(" NAK") {
            if let Some(list) = c.split(':').nth(1).or_else(|| c.split_whitespace().last()) {
                for cap in list.split_whitespace().map(|v| v.trim()) {
                    if !cap.is_empty() {
                        server.enabled_caps.remove(cap);
                    }
                }
            }
        }
    }

    fn show_connection_overview(ui: &mut Ui, server: &IrcServerStatus) {
        egui::Grid::new("irc_status_conn_grid")
            .num_columns(2)
            .striped(true)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                status_row(ui, "Network", opt_text(server.network_name.clone()));
                status_row(ui, "Server hostname", server.configured_host.clone());
                status_row(
                    ui,
                    "Connected server",
                    opt_text(server.connected_server.clone()),
                );
                status_row(ui, "Port", server.configured_port.to_string());
                status_row(ui, "TLS enabled", yes_no(server.tls_enabled));
                status_row(ui, "TLS cipher", "n/a");
                status_row(ui, "Certificate status", "n/a");
                status_row(ui, "Local IP", "n/a");
                status_row(ui, "Remote IP", opt_text(server.remote_ip.clone()));
                status_row(ui, "Connection uptime", fmt_uptime(server.connected_since));
                status_row(
                    ui,
                    "Reconnect attempts",
                    server.reconnect_attempts.to_string(),
                );
                status_row(
                    ui,
                    "Dropped connections",
                    server.dropped_connections.to_string(),
                );
                status_row(ui, "Ping latency", opt_ms(server.last_ping_ms));
                status_row(ui, "Last PING", opt_unix_ts(server.last_ping_unix));
                status_row(ui, "Last PONG", opt_unix_ts(server.last_pong_unix));
                status_row(
                    ui,
                    "Current nickname",
                    opt_text(server.current_nick.clone()),
                );
                status_row(ui, "Username", opt_text(server.username.clone()));
                status_row(ui, "Realname", opt_text(server.realname.clone()));
                status_row(ui, "Account name", opt_text(server.account_name.clone()));
                status_row(
                    ui,
                    "NickServ identified",
                    yes_no(server.nickserv_identified),
                );
                status_row(ui, "User modes", opt_text(server.user_modes.clone()));
                status_row(ui, "SASL enabled", yes_no(server.sasl_enabled));
                status_row(
                    ui,
                    "SASL mechanism",
                    opt_text(server.sasl_mechanism.clone()),
                );
            });
    }

    fn show_channel_state(
        ui: &mut Ui,
        server: &IrcServerStatus,
        active_channel: Option<&ChannelId>,
        sel: &IrcServerKey,
    ) {
        let preferred = active_channel.and_then(|ch| {
            IrcServerKey::from_channel(ch).and_then(|(k, channel_name)| {
                if &k == sel {
                    Some(channel_name)
                } else {
                    None
                }
            })
        });

        let mut names: Vec<&String> = server
            .channels
            .keys()
            .filter(|name| name.as_str() != IRC_SERVER_CONTROL_CHANNEL)
            .collect();
        names.sort();

        let channel_name = preferred.or_else(|| names.first().map(|s| (*s).clone()));
        let Some(channel_name) = channel_name else {
            ui.label(
                RichText::new("No IRC channel selected on this server yet.").color(t::TEXT_MUTED),
            );
            return;
        };
        let Some(ch) = server.channels.get(&channel_name) else {
            ui.label(RichText::new("No channel status available yet.").color(t::TEXT_MUTED));
            return;
        };

        ui.label(
            RichText::new(format!("Inspecting #{}", channel_name))
                .font(t::small())
                .color(t::ACCENT),
        );
        ui.add_space(4.0);

        let modes = ch.modes.clone().unwrap_or_default();
        egui::Grid::new("irc_status_channel_grid")
            .num_columns(2)
            .striped(true)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                status_row(ui, "Channel", format!("#{channel_name}"));
                status_row(ui, "Channel modes", opt_text(ch.modes.clone()));
                status_row(ui, "Topic", opt_text(ch.topic.clone()));
                status_row(ui, "Topic setter", opt_text(ch.topic_setter.clone()));
                status_row(ui, "Topic set time", opt_unix_ts(ch.topic_set_unix));
                status_row(ui, "Channel creation time", opt_unix_ts(ch.creation_unix));
                status_row(ui, "Channel key", yes_no(ch.joined_with_key));
                status_row(
                    ui,
                    "Channel limit",
                    ch.user_limit
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "n/a".to_owned()),
                );
                status_row(ui, "Total users", ch.users_total.to_string());
                status_row(ui, "Ops (@/~/&)", ch.ops_count.to_string());
                status_row(ui, "Halfops (%)", ch.halfops_count.to_string());
                status_row(ui, "Voiced (+)", ch.voiced_count.to_string());
                status_row(ui, "Normal users", ch.normal_count.to_string());
                status_row(
                    ui,
                    "Your privilege",
                    ch.your_privilege
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "normal".to_owned()),
                );
                status_row(ui, "Are you banned", "n/a");
                status_row(ui, "Are you quieted", "n/a");
                status_row(ui, "+R registered only", yes_no(mode_enabled(&modes, 'R')));
                status_row(ui, "+M moderated", yes_no(mode_enabled(&modes, 'M')));
                status_row(ui, "+i invite only", yes_no(mode_enabled(&modes, 'i')));
                status_row(ui, "+k key required", yes_no(mode_enabled(&modes, 'k')));
                status_row(ui, "+b list count", ch.ban_list_count.to_string());
                status_row(ui, "+e exceptions", ch.except_list_count.to_string());
                status_row(ui, "+q quiet list", ch.quiet_list_count.to_string());
            });
    }

    fn show_protocol_status(ui: &mut Ui, server: &IrcServerStatus) {
        egui::Grid::new("irc_status_proto_grid")
            .num_columns(2)
            .striped(true)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                status_row(
                    ui,
                    "CAP LS results",
                    if server.cap_ls.is_empty() {
                        "n/a".to_owned()
                    } else {
                        server.cap_ls.iter().cloned().collect::<Vec<_>>().join(", ")
                    },
                );
                status_row(
                    ui,
                    "Enabled capabilities",
                    if server.enabled_caps.is_empty() {
                        "n/a".to_owned()
                    } else {
                        server
                            .enabled_caps
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    },
                );
                status_row(ui, "SASL negotiated", yes_no(server.sasl_enabled));
                status_row(
                    ui,
                    "server-time enabled",
                    yes_no(server.enabled_caps.contains("server-time")),
                );
                status_row(
                    ui,
                    "message-tags enabled",
                    yes_no(server.enabled_caps.contains("message-tags")),
                );
                status_row(
                    ui,
                    "multi-prefix enabled",
                    yes_no(server.enabled_caps.contains("multi-prefix")),
                );
                status_row(
                    ui,
                    "batch enabled",
                    yes_no(server.enabled_caps.contains("batch")),
                );
                status_row(ui, "Last numeric", opt_text(server.last_numeric.clone()));
                status_row(ui, "MOTD received", yes_no(server.motd_received));
                status_row(ui, "WHOIS cache entries", server.whois.len().to_string());
                status_row(
                    ui,
                    "MONITOR list size",
                    server
                        .monitor_list_size
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "n/a".to_owned()),
                );
                status_row(ui, "Outgoing queue size", "n/a");
                status_row(ui, "Throttling active", "n/a");
            });
    }

    fn show_traffic_panel(ui: &mut Ui, server: &IrcServerStatus) {
        let uptime_secs = server
            .connected_since
            .map(|s| s.elapsed().as_secs_f32())
            .unwrap_or(0.0);
        let recv_rate = if uptime_secs > 0.0 {
            server.messages_received as f32 / uptime_secs
        } else {
            0.0
        };
        egui::Grid::new("irc_status_traffic_grid")
            .num_columns(2)
            .striped(true)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                status_row(ui, "Bytes sent", server.bytes_sent.to_string());
                status_row(ui, "Bytes received", server.bytes_received.to_string());
                status_row(ui, "Messages sent", server.messages_sent.to_string());
                status_row(
                    ui,
                    "Messages received",
                    server.messages_received.to_string(),
                );
                status_row(
                    ui,
                    "PRIVMSG count",
                    (server.privmsg_sent + server.privmsg_received).to_string(),
                );
                status_row(ui, "NOTICE count", server.notice_count.to_string());
                status_row(ui, "JOIN/PART count", server.join_part_count.to_string());
                status_row(ui, "Errors received", server.error_count.to_string());
                status_row(ui, "CTCP received", server.ctcp_received.to_string());
                status_row(ui, "CTCP sent", server.ctcp_sent.to_string());
                status_row(ui, "Messages per second", format!("{recv_rate:.2}"));
                status_row(ui, "Average ping", opt_ms(server.avg_ping_ms));
                status_row(ui, "Highest ping", opt_ms(server.max_ping_ms));
                status_row(ui, "Reconnect count", server.reconnect_attempts.to_string());
                status_row(
                    ui,
                    "Dropped connections",
                    server.dropped_connections.to_string(),
                );
            });
    }

    fn show_security_panel(ui: &mut Ui, server: &IrcServerStatus) {
        egui::Grid::new("irc_status_security_grid")
            .num_columns(2)
            .striped(true)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                status_row(ui, "TLS active", yes_no(server.tls_enabled));
                status_row(ui, "Cipher suite", "n/a");
                status_row(ui, "Cert fingerprint (SHA256)", "n/a");
                status_row(ui, "Certificate issuer", "n/a");
                status_row(ui, "Certificate expiration", "n/a");
                status_row(ui, "Hostname verification", "n/a");
                status_row(ui, "SASL status", yes_no(server.sasl_enabled));
                status_row(ui, "NickServ logged in", yes_no(server.nickserv_identified));
                let user_modes = server.user_modes.clone().unwrap_or_default();
                status_row(ui, "+i user mode", yes_no(mode_enabled(&user_modes, 'i')));
            });
    }

    fn show_event_log(ui: &mut Ui, server: &IrcServerStatus, key: &IrcServerKey) {
        if server.event_log.is_empty() {
            ui.label(RichText::new("No IRC lifecycle/protocol events yet.").color(t::TEXT_MUTED));
            return;
        }
        let id = egui::Id::new("irc_status_event_log").with(key.label());
        egui::ScrollArea::vertical()
            .id_salt(id)
            .max_height(240.0)
            .show(ui, |ui| {
                for line in server.event_log.iter().rev().take(220) {
                    let color = if line.contains("[error]") {
                        Color32::from_rgb(240, 130, 130)
                    } else if line.contains("[conn]") {
                        t::ACCENT
                    } else if line.contains("[auth]") {
                        Color32::from_rgb(215, 190, 120)
                    } else {
                        t::TEXT_SECONDARY
                    };
                    ui.label(RichText::new(line).font(t::small()).color(color));
                }
            });
    }

    fn show_user_inspect(ui: &mut Ui, server: &mut IrcServerStatus, key: &IrcServerKey) {
        if server.whois.is_empty() {
            ui.label(
                RichText::new("No WHOIS cache yet. Run `/whois <nick>` on this server.")
                    .color(t::TEXT_MUTED),
            );
            return;
        }

        let mut nicks: Vec<String> = server.whois.keys().cloned().collect();
        nicks.sort_by_key(|n| n.to_ascii_lowercase());
        if server.selected_whois_nick.is_none() {
            server.selected_whois_nick = nicks.first().cloned();
        }

        ui.horizontal(|ui| {
            ui.label(RichText::new("Nick").color(t::TEXT_SECONDARY));
            ComboBox::from_id_salt(egui::Id::new("irc_status_whois_select").with(key.label()))
                .selected_text(
                    server
                        .selected_whois_nick
                        .clone()
                        .unwrap_or_else(|| "Select nick".to_owned()),
                )
                .show_ui(ui, |ui| {
                    for nick in &nicks {
                        ui.selectable_value(
                            &mut server.selected_whois_nick,
                            Some(nick.clone()),
                            nick,
                        );
                    }
                });
        });

        let Some(nick) = server.selected_whois_nick.clone() else {
            return;
        };
        let Some(entry) = server.whois.get(&nick) else {
            return;
        };

        egui::Grid::new("irc_status_user_grid")
            .num_columns(2)
            .striped(true)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                status_row(ui, "Nickname", entry.nick.clone());
                status_row(ui, "Username", opt_text(entry.username.clone()));
                status_row(ui, "Hostmask", opt_text(entry.host.clone()));
                status_row(ui, "Account name", opt_text(entry.account.clone()));
                status_row(ui, "Realname", opt_text(entry.realname.clone()));
                status_row(
                    ui,
                    "Idle time",
                    entry
                        .idle_seconds
                        .map(fmt_duration_secs)
                        .unwrap_or_else(|| "n/a".to_owned()),
                );
                status_row(ui, "Signon time", opt_unix_ts(entry.signon_unix));
                status_row(ui, "Channel privilege", "n/a");
                status_row(ui, "Is operator", yes_no(entry.is_operator));
                status_row(ui, "Is away", yes_no(entry.is_away));
                status_row(ui, "Away message", opt_text(entry.away_message.clone()));
                status_row(
                    ui,
                    "Shared channels",
                    if entry.shared_channels.is_empty() {
                        "n/a".to_owned()
                    } else {
                        entry.shared_channels.join(" ")
                    },
                );
            });
    }
}

fn parse_numeric_line(text: &str) -> Option<(&str, &str)> {
    let body = text.strip_prefix('[')?;
    let end = body.find(']')?;
    let code = &body[..end];
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let rem = body[end + 1..].trim_start();
    Some((code, rem))
}

fn parse_host_line(server: &mut IrcServerStatus, line: &str) {
    let body = line.trim();
    if let Some(after) = body.strip_prefix("Your host is ") {
        if let Some(host) = after.split('[').next().map(str::trim) {
            if !host.is_empty() {
                server.connected_server = Some(host.to_owned());
            }
        }
        if let Some(bracketed) = after
            .split('[')
            .nth(1)
            .and_then(|s| s.split(']').next())
            .map(str::trim)
        {
            let remote = bracketed
                .split('/')
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            server.remote_ip = remote.map(str::to_owned);
        }
    }
}

fn parse_mode_setter_line(line: &str) -> Option<(String, String, String, String)> {
    let (left, target) = line.split_once(" on ")?;
    let (actor, mode_and_args) = left.split_once(" set mode ")?;
    let mut parts = mode_and_args.split_whitespace();
    let mode = parts.next()?.to_owned();
    let args = parts.collect::<Vec<_>>().join(" ");
    Some((
        actor.trim().to_owned(),
        mode.trim().to_owned(),
        target.trim().to_owned(),
        args.trim().to_owned(),
    ))
}

fn extract_irc_feature_value(body: &str, key: &str) -> Option<String> {
    body.split_whitespace().find_map(|part| {
        let (k, v) = part.split_once('=')?;
        if k.eq_ignore_ascii_case(key) {
            Some(v.trim_end_matches(',').to_owned())
        } else {
            None
        }
    })
}

fn split_middle_and_trailing(body: &str) -> (&str, Option<&str>) {
    if let Some((m, t)) = body.split_once("—") {
        return (m.trim(), Some(t.trim()));
    }
    (body.trim(), None)
}

fn normalize_channel_token(raw: &str) -> Option<String> {
    let token = raw.trim().trim_start_matches(['@', '+', '%', '&', '~']);
    if !token.starts_with('#') {
        return None;
    }
    let out = token.trim_start_matches('#').to_ascii_lowercase();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn mode_enabled(modes: &str, mode: char) -> bool {
    let mut enabled = false;
    let mut sign = '+';
    for ch in modes.chars() {
        if ch == '+' || ch == '-' {
            sign = ch;
            continue;
        }
        if ch == mode {
            enabled = sign == '+';
        }
    }
    enabled
}

fn looks_like_raw_irc_protocol_line(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    let cmd = trimmed.split_whitespace().next().unwrap_or("").trim();
    if cmd.is_empty() {
        return false;
    }
    if cmd.len() == 3 && cmd.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    if !cmd.bytes().all(|b| b.is_ascii_uppercase()) {
        return false;
    }
    matches!(
        cmd,
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

fn is_ctcp_body(raw: &str) -> bool {
    raw.contains("\u{0001}ACTION")
        || raw.contains("\u{0001}PING")
        || raw.contains("\u{0001}VERSION")
}

fn status_row(ui: &mut Ui, key: &str, value: impl Into<String>) {
    ui.label(RichText::new(key).color(t::TEXT_MUTED));
    ui.label(RichText::new(value.into()).color(t::TEXT_PRIMARY));
    ui.end_row();
}

fn status_chip(ui: &mut Ui, text: &str, fg: Color32) {
    egui::Frame::new()
        .fill(t::BG_RAISED)
        .stroke(egui::Stroke::new(1.0, t::BORDER_SUBTLE))
        .corner_radius(t::RADIUS_SM)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.label(RichText::new(text).font(t::small()).color(fg));
        });
}

fn yes_no(v: bool) -> &'static str {
    if v {
        "yes"
    } else {
        "no"
    }
}

fn opt_text(v: Option<String>) -> String {
    v.filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "n/a".to_owned())
}

fn opt_ms(v: Option<f32>) -> String {
    v.map(|n| format!("{n:.1} ms"))
        .unwrap_or_else(|| "n/a".to_owned())
}

fn opt_unix_ts(ts: Option<i64>) -> String {
    ts.and_then(|v| Utc.timestamp_opt(v, 0).single())
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "n/a".to_owned())
}

fn fmt_duration_secs(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

fn fmt_uptime(connected_since: Option<Instant>) -> String {
    connected_since
        .map(|t| fmt_duration_secs(t.elapsed().as_secs()))
        .unwrap_or_else(|| "n/a".to_owned())
}

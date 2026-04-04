use std::collections::HashSet;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tracing::{debug, info, warn};

const EVENTSUB_WS_URL: &str = "wss://eventsub.wss.twitch.tv/ws";
const RECONNECT_BACKOFF_SECS: &[u64] = &[1, 2, 5, 10, 20, 30];

/// EventSub event deduplication cache size (number of recent event IDs to track).
const EVENT_DEDUP_CACHE_SIZE: usize = 10000;

#[derive(Debug, Clone)]
pub enum EventSubCommand {
    SetAuth {
        token: String,
        client_id: String,
        user_id: String,
    },
    ClearAuth,
    WatchChannel {
        broadcaster_id: String,
    },
    UnwatchChannel {
        broadcaster_id: String,
    },
}

#[derive(Debug, Clone)]
pub enum EventSubNoticeKind {
    Follow {
        user_login: String,
    },
    Subscribe {
        user_login: String,
        tier: String,
        is_gift: bool,
    },
    SubscriptionGift {
        gifter_login: Option<String>,
        tier: String,
        total: Option<u32>,
    },
    Raid {
        from_login: String,
        viewers: u32,
    },
    ChannelChatUserMessageHold {
        message_id: String,
        user_id: String,
        user_login: String,
        user_name: String,
        text: String,
    },
    ChannelChatUserMessageUpdate {
        message_id: String,
        user_id: String,
        user_login: String,
        user_name: String,
        text: String,
        status: String,
    },
    ChannelPointsRedemption {
        user_login: String,
        reward_title: String,
        cost: u32,
        reward_id: Option<String>,
        redemption_id: Option<String>,
        user_input: Option<String>,
        status: Option<String>,
        is_update: bool,
    },
    PollLifecycle {
        title: String,
        phase: String,
        status: Option<String>,
        details: Option<String>,
    },
    PredictionLifecycle {
        title: String,
        phase: String,
        status: Option<String>,
        details: Option<String>,
    },
    AutoModMessageHold {
        message_id: String,
        sender_user_id: String,
        sender_login: String,
        text: String,
        reason: Option<String>,
    },
    AutoModMessageUpdate {
        message_id: String,
        status: String,
    },
    UnbanRequestCreate {
        request_id: String,
        user_id: String,
        user_login: String,
        text: Option<String>,
        created_at: Option<String>,
    },
    UnbanRequestResolve {
        request_id: String,
        status: String,
    },
    ChannelBan {
        user_login: String,
        reason: Option<String>,
        ends_at: Option<String>,
    },
    ChannelUnban {
        user_login: String,
    },
    SuspiciousUserMessage {
        user_id: String,
        user_login: String,
        user_name: String,
        low_trust_status: String,
        ban_evasion_evaluation: Option<String>,
        shared_ban_channel_ids: Vec<String>,
        types: Vec<String>,
        text: String,
    },
    SuspiciousUserUpdate {
        user_id: String,
        user_login: String,
        user_name: String,
        moderator_user_id: String,
        moderator_login: String,
        moderator_name: String,
        low_trust_status: String,
    },
    UserWhisperMessage {
        from_user_id: String,
        from_user_login: String,
        from_user_name: String,
        to_user_id: String,
        to_user_login: String,
        to_user_name: String,
        whisper_id: String,
        text: String,
    },
    ModerationAction {
        moderator_login: String,
        action: String,
        target_login: Option<String>,
        target_message_id: Option<String>,
        source_channel_login: Option<String>,
    },
    StreamOnline,
    StreamOffline,
}

#[derive(Debug, Clone)]
pub struct EventSubNotice {
    /// EventSub metadata.message_id; stable per notification event.
    pub event_id: Option<String>,
    pub broadcaster_id: String,
    pub broadcaster_login: Option<String>,
    pub kind: EventSubNoticeKind,
}

#[derive(Debug, Clone)]
pub enum EventSubEvent {
    Connected { resumed: bool },
    Reconnecting { attempt: u32 },
    BackfillRequested,
    Notice(EventSubNotice),
    Error(String),
}

#[derive(Debug, Clone)]
struct EventSubAuth {
    token: String,
    client_id: String,
    user_id: String,
}

pub struct EventSubSession {
    event_tx: mpsc::Sender<EventSubEvent>,
    cmd_rx: mpsc::Receiver<EventSubCommand>,
    auth: Option<EventSubAuth>,
    watched_broadcasters: HashSet<String>,
    http: reqwest::Client,
    resumed_once: bool,
    /// Deduplication cache: tracks recent event IDs to prevent duplicate processing.
    seen_event_ids: LruCache,
}

/// Simple LRU cache for event ID deduplication.
struct LruCache {
    items: Vec<String>,
    capacity: usize,
}

impl LruCache {
    fn new(capacity: usize) -> Self {
        Self {
            items: Vec::with_capacity(capacity),
            capacity,
        }
    }

    fn contains(&self, key: &str) -> bool {
        self.items.iter().any(|s| s == key)
    }

    fn insert(&mut self, key: String) {
        if self.items.len() >= self.capacity {
            self.items.remove(0);
        }
        self.items.push(key);
    }
}

impl EventSubSession {
    pub fn new(
        event_tx: mpsc::Sender<EventSubEvent>,
        cmd_rx: mpsc::Receiver<EventSubCommand>,
    ) -> Self {
        Self {
            event_tx,
            cmd_rx,
            auth: None,
            watched_broadcasters: HashSet::new(),
            http: reqwest::Client::new(),
            resumed_once: false,
            seen_event_ids: LruCache::new(EVENT_DEDUP_CACHE_SIZE),
        }
    }

    async fn emit(&self, evt: EventSubEvent) {
        let _ = self.event_tx.send(evt).await;
    }

    pub async fn run(mut self) {
        let mut attempt: u32 = 0;
        let mut connect_url = EVENTSUB_WS_URL.to_owned();

        loop {
            self.emit(EventSubEvent::Reconnecting { attempt }).await;

            match self.connect_once(&connect_url).await {
                Ok(EventSubConnectOutcome::Stop) => {
                    info!("EventSub loop stopping");
                    return;
                }
                Ok(EventSubConnectOutcome::Reconnect {
                    reconnect_url,
                    immediate,
                }) => {
                    connect_url = reconnect_url.unwrap_or_else(|| EVENTSUB_WS_URL.to_owned());
                    if immediate {
                        attempt = 0;
                        continue;
                    }
                }
                Err(e) => {
                    self.emit(EventSubEvent::Error(format!(
                        "EventSub connection error: {e}"
                    )))
                    .await;
                }
            }

            let delay = RECONNECT_BACKOFF_SECS
                .get(attempt as usize)
                .copied()
                .unwrap_or(*RECONNECT_BACKOFF_SECS.last().unwrap_or(&30));
            tokio::time::sleep(Duration::from_secs(delay)).await;
            attempt = attempt.saturating_add(1);
        }
    }

    async fn connect_once(&mut self, url: &str) -> Result<EventSubConnectOutcome, String> {
        info!("Connecting EventSub websocket: {url}");
        let (ws, _) = connect_async(url)
            .await
            .map_err(|e| format!("connect failed: {e}"))?;
        let (_sink, mut stream) = ws.split();

        let mut session_id: Option<String> = None;

        loop {
            tokio::select! {
                maybe_cmd = self.cmd_rx.recv() => {
                    let Some(cmd) = maybe_cmd else {
                        return Ok(EventSubConnectOutcome::Stop);
                    };
                    match cmd {
                        EventSubCommand::SetAuth { token, client_id, user_id } => {
                            let bare = token.strip_prefix("oauth:").unwrap_or(&token).trim().to_owned();
                            if bare.is_empty() || client_id.trim().is_empty() || user_id.trim().is_empty() {
                                self.auth = None;
                                continue;
                            }
                            self.auth = Some(EventSubAuth {
                                token: bare,
                                client_id: client_id.trim().to_owned(),
                                user_id: user_id.trim().to_owned(),
                            });
                            if let Some(ref sid) = session_id {
                                self.subscribe_all(sid).await;
                            }
                        }
                        EventSubCommand::ClearAuth => {
                            self.auth = None;
                        }
                        EventSubCommand::WatchChannel { broadcaster_id } => {
                            let bid = broadcaster_id.trim();
                            if bid.is_empty() {
                                continue;
                            }
                            self.watched_broadcasters.insert(bid.to_owned());
                            if let Some(ref sid) = session_id {
                                self.subscribe_channel(sid, bid).await;
                            }
                        }
                        EventSubCommand::UnwatchChannel { broadcaster_id } => {
                            self.watched_broadcasters.remove(broadcaster_id.trim());
                        }
                    }
                }
                maybe_msg = stream.next() => {
                    match maybe_msg {
                        None => {
                            warn!("EventSub websocket closed");
                            return Ok(EventSubConnectOutcome::Reconnect {
                                reconnect_url: None,
                                immediate: false,
                            });
                        }
                        Some(Err(e)) => {
                            return Err(format!("websocket read failed: {e}"));
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                            let parsed: Value = match serde_json::from_str(&text) {
                                Ok(v) => v,
                                Err(e) => {
                                    debug!("EventSub JSON parse error: {e}; payload={text}");
                                    continue;
                                }
                            };

                            let message_type = parsed
                                .get("metadata")
                                .and_then(|m| m.get("message_type"))
                                .and_then(Value::as_str)
                                .unwrap_or("");

                            match message_type {
                                "session_welcome" => {
                                    let sid = parsed
                                        .get("payload")
                                        .and_then(|p| p.get("session"))
                                        .and_then(|s| s.get("id"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_owned();
                                    if sid.is_empty() {
                                        continue;
                                    }

                                    session_id = Some(sid.clone());
                                    self.emit(EventSubEvent::Connected {
                                        resumed: self.resumed_once,
                                    }).await;
                                    self.emit(EventSubEvent::BackfillRequested).await;
                                    self.resumed_once = true;
                                    self.subscribe_all(&sid).await;
                                }
                                "session_keepalive" => {
                                    // No-op; keepalive proves the socket/session is healthy.
                                }
                                "session_reconnect" => {
                                    let reconnect_url = parsed
                                        .get("payload")
                                        .and_then(|p| p.get("session"))
                                        .and_then(|s| s.get("reconnect_url"))
                                        .and_then(Value::as_str)
                                        .map(str::to_owned);

                                    return Ok(EventSubConnectOutcome::Reconnect {
                                        reconnect_url,
                                        immediate: true,
                                    });
                                }
                                "revocation" => {
                                    let reason = parsed
                                        .get("payload")
                                        .and_then(|p| p.get("subscription"))
                                        .and_then(|s| s.get("status"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("revoked")
                                        .to_owned();
                                    self.emit(EventSubEvent::Error(format!(
                                        "EventSub subscription revoked: {reason}"
                                    )))
                                    .await;
                                }
                                "notification" => {
                                    let sub_type = parsed
                                        .get("metadata")
                                        .and_then(|m| m.get("subscription_type"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("");

                                    let event_id = parsed
                                        .get("metadata")
                                        .and_then(|m| m.get("message_id"))
                                        .and_then(Value::as_str);

                                    // Deduplicate: skip if we've already seen this event ID.
                                    if let Some(eid) = event_id {
                                        if self.seen_event_ids.contains(eid) {
                                            debug!("Skipping duplicate EventSub notification: {eid}");
                                            continue;
                                        }
                                        self.seen_event_ids.insert(eid.to_owned());
                                    }

                                    if let Some(notice) = parse_notice(&parsed, sub_type) {
                                        self.emit(EventSubEvent::Notice(notice)).await;
                                    }
                                }
                                other => {
                                    debug!("Unhandled EventSub message type: {other}");
                                }
                            }
                        }
                        Some(Ok(_)) => {
                            // Ignore binary/ping/pong frames.
                        }
                    }
                }
            }
        }
    }

    async fn subscribe_all(&self, session_id: &str) {
        if session_id.trim().is_empty() {
            return;
        }
        for broadcaster_id in &self.watched_broadcasters {
            self.subscribe_channel(session_id, broadcaster_id).await;
        }
    }

    async fn subscribe_channel(&self, session_id: &str, broadcaster_id: &str) {
        let Some(auth) = self.auth.as_ref() else {
            return;
        };

        let specs = subscription_specs(broadcaster_id, &auth.user_id);
        for spec in specs {
            let body = json!({
                "type": spec.kind,
                "version": spec.version,
                "condition": spec.condition,
                "transport": {
                    "method": "websocket",
                    "session_id": session_id,
                }
            });

            let resp = self
                .http
                .post("https://api.twitch.tv/helix/eventsub/subscriptions")
                .header("Authorization", format!("Bearer {}", auth.token))
                .header("Client-Id", &auth.client_id)
                .json(&body)
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    debug!("EventSub subscribed: {} for {}", spec.kind, broadcaster_id);
                }
                Ok(r) if r.status() == StatusCode::CONFLICT => {
                    debug!(
                        "EventSub already subscribed (conflict): {} for {}",
                        spec.kind, broadcaster_id
                    );
                }
                Ok(r) => {
                    let status = r.status();
                    let body = r.text().await.unwrap_or_default();
                    warn!(
                        "EventSub subscribe failed {} for {}: HTTP {} - {}",
                        spec.kind, broadcaster_id, status, body
                    );
                }
                Err(e) => {
                    warn!(
                        "EventSub subscribe request failed {} for {}: {}",
                        spec.kind, broadcaster_id, e
                    );
                }
            }
        }
    }
}

enum EventSubConnectOutcome {
    Reconnect {
        reconnect_url: Option<String>,
        immediate: bool,
    },
    Stop,
}

struct SubscriptionSpec {
    kind: &'static str,
    version: &'static str,
    condition: Value,
}

fn subscription_specs(broadcaster_id: &str, moderator_user_id: &str) -> Vec<SubscriptionSpec> {
    let bid = broadcaster_id.trim();
    let mid = moderator_user_id.trim();
    if bid.is_empty() {
        return Vec::new();
    }

    let mut out = vec![
        SubscriptionSpec {
            kind: "stream.online",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
        },
        SubscriptionSpec {
            kind: "stream.offline",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
        },
        SubscriptionSpec {
            kind: "channel.subscribe",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
        },
        SubscriptionSpec {
            kind: "channel.subscription.gift",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
        },
        SubscriptionSpec {
            kind: "channel.chat.user_message_hold",
            version: "1",
            condition: json!({
                "broadcaster_user_id": bid,
                "user_id": mid,
            }),
        },
        SubscriptionSpec {
            kind: "channel.chat.user_message_update",
            version: "1",
            condition: json!({
                "broadcaster_user_id": bid,
                "user_id": mid,
            }),
        },
        SubscriptionSpec {
            kind: "channel.raid",
            version: "1",
            condition: json!({"to_broadcaster_user_id": bid}),
        },
        SubscriptionSpec {
            kind: "channel.channel_points_custom_reward_redemption.add",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
        },
        SubscriptionSpec {
            kind: "channel.channel_points_custom_reward_redemption.update",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
        },
        SubscriptionSpec {
            kind: "channel.poll.begin",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
        },
        SubscriptionSpec {
            kind: "channel.poll.progress",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
        },
        SubscriptionSpec {
            kind: "channel.poll.end",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
        },
        SubscriptionSpec {
            kind: "channel.prediction.begin",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
        },
        SubscriptionSpec {
            kind: "channel.prediction.progress",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
        },
        SubscriptionSpec {
            kind: "channel.prediction.lock",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
        },
        SubscriptionSpec {
            kind: "channel.prediction.end",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
        },
    ];

    if !mid.is_empty() {
        out.push(SubscriptionSpec {
            kind: "channel.follow",
            version: "2",
            condition: json!({
                "broadcaster_user_id": bid,
                "moderator_user_id": mid,
            }),
        });

        out.push(SubscriptionSpec {
            kind: "automod.message.hold",
            version: "2",
            condition: json!({
                "broadcaster_user_id": bid,
                "moderator_user_id": mid,
            }),
        });
        out.push(SubscriptionSpec {
            kind: "automod.message.update",
            version: "1",
            condition: json!({
                "broadcaster_user_id": bid,
                "moderator_user_id": mid,
            }),
        });
        out.push(SubscriptionSpec {
            kind: "channel.unban_request.create",
            version: "1",
            condition: json!({
                "broadcaster_user_id": bid,
                "moderator_user_id": mid,
            }),
        });
        out.push(SubscriptionSpec {
            kind: "channel.unban_request.resolve",
            version: "1",
            condition: json!({
                "broadcaster_user_id": bid,
                "moderator_user_id": mid,
            }),
        });
        out.push(SubscriptionSpec {
            kind: "channel.ban",
            version: "1",
            condition: json!({
                "broadcaster_user_id": bid,
                "moderator_user_id": mid,
            }),
        });
        out.push(SubscriptionSpec {
            kind: "channel.unban",
            version: "1",
            condition: json!({
                "broadcaster_user_id": bid,
                "moderator_user_id": mid,
            }),
        });
        out.push(SubscriptionSpec {
            kind: "channel.moderate",
            version: "2",
            condition: json!({
                "broadcaster_user_id": bid,
                "moderator_user_id": mid,
            }),
        });
        out.push(SubscriptionSpec {
            kind: "channel.suspicious_user.message",
            version: "1",
            condition: json!({
                "broadcaster_user_id": bid,
                "moderator_user_id": mid,
            }),
        });
        out.push(SubscriptionSpec {
            kind: "channel.suspicious_user.update",
            version: "1",
            condition: json!({
                "broadcaster_user_id": bid,
                "moderator_user_id": mid,
            }),
        });
        out.push(SubscriptionSpec {
            kind: "user.whisper.message",
            version: "1",
            condition: json!({
                "user_id": mid,
            }),
        });
    }

    out
}

fn parse_notice(root: &Value, sub_type: &str) -> Option<EventSubNotice> {
    let payload = root.get("payload")?;
    let event = payload.get("event")?;
    let subscription = payload.get("subscription");
    let event_id = root
        .get("metadata")
        .and_then(|m| m.get("message_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let broadcaster_id = event
        .get("broadcaster_user_id")
        .and_then(Value::as_str)
        .or_else(|| event.get("to_broadcaster_user_id").and_then(Value::as_str))
        .or_else(|| {
            subscription
                .and_then(|s| s.get("condition"))
                .and_then(|c| c.get("broadcaster_user_id"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            subscription
                .and_then(|s| s.get("condition"))
                .and_then(|c| c.get("to_broadcaster_user_id"))
                .and_then(Value::as_str)
        })?
        .to_owned();

    let broadcaster_login = event
        .get("broadcaster_user_login")
        .and_then(Value::as_str)
        .or_else(|| {
            event
                .get("to_broadcaster_user_login")
                .and_then(Value::as_str)
        })
        .map(str::to_owned);

    let kind = match sub_type {
        "channel.follow" => EventSubNoticeKind::Follow {
            user_login: pick_non_empty(
                event.get("user_login").and_then(Value::as_str),
                event.get("user_name").and_then(Value::as_str),
                "someone",
            )
            .to_owned(),
        },
        "channel.subscribe" => EventSubNoticeKind::Subscribe {
            user_login: pick_non_empty(
                event.get("user_login").and_then(Value::as_str),
                event.get("user_name").and_then(Value::as_str),
                "someone",
            )
            .to_owned(),
            tier: normalize_tier(event.get("tier").and_then(Value::as_str)),
            is_gift: event
                .get("is_gift")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "channel.subscription.gift" => EventSubNoticeKind::SubscriptionGift {
            gifter_login: pick_optional_non_empty(
                event.get("user_login").and_then(Value::as_str),
                event.get("user_name").and_then(Value::as_str),
            )
            .map(str::to_owned),
            tier: normalize_tier(event.get("tier").and_then(Value::as_str)),
            total: event
                .get("total")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .or_else(|| {
                    event
                        .get("cumulative_total")
                        .and_then(Value::as_u64)
                        .and_then(|v| u32::try_from(v).ok())
                }),
        },
        "channel.raid" => EventSubNoticeKind::Raid {
            from_login: pick_non_empty(
                event
                    .get("from_broadcaster_user_login")
                    .and_then(Value::as_str),
                event
                    .get("from_broadcaster_user_name")
                    .and_then(Value::as_str),
                "another channel",
            )
            .to_owned(),
            viewers: event
                .get("viewers")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(0),
        },
        "channel.chat.user_message_hold" => EventSubNoticeKind::ChannelChatUserMessageHold {
            message_id: pick_non_empty(
                event.get("message_id").and_then(Value::as_str),
                event.get("id").and_then(Value::as_str),
                "",
            )
            .to_owned(),
            user_id: pick_non_empty(event.get("user_id").and_then(Value::as_str), Some(""), "")
                .to_owned(),
            user_login: pick_non_empty(
                event.get("user_login").and_then(Value::as_str),
                event.get("user_name").and_then(Value::as_str),
                "unknown",
            )
            .to_owned(),
            user_name: pick_non_empty(
                event.get("user_name").and_then(Value::as_str),
                event.get("user_login").and_then(Value::as_str),
                "unknown",
            )
            .to_owned(),
            text: event
                .get("message")
                .and_then(|m| m.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        },
        "channel.chat.user_message_update" => EventSubNoticeKind::ChannelChatUserMessageUpdate {
            message_id: pick_non_empty(
                event.get("message_id").and_then(Value::as_str),
                event.get("id").and_then(Value::as_str),
                "",
            )
            .to_owned(),
            user_id: pick_non_empty(event.get("user_id").and_then(Value::as_str), Some(""), "")
                .to_owned(),
            user_login: pick_non_empty(
                event.get("user_login").and_then(Value::as_str),
                event.get("user_name").and_then(Value::as_str),
                "unknown",
            )
            .to_owned(),
            user_name: pick_non_empty(
                event.get("user_name").and_then(Value::as_str),
                event.get("user_login").and_then(Value::as_str),
                "unknown",
            )
            .to_owned(),
            text: event
                .get("message")
                .and_then(|m| m.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            status: pick_non_empty(
                event.get("status").and_then(Value::as_str),
                Some(""),
                "unknown",
            )
            .to_owned(),
        },
        "channel.channel_points_custom_reward_redemption.add"
        | "channel.channel_points_custom_reward_redemption.update" => {
            let reward = event.get("reward");
            EventSubNoticeKind::ChannelPointsRedemption {
                user_login: pick_non_empty(
                    event.get("user_login").and_then(Value::as_str),
                    event.get("user_name").and_then(Value::as_str),
                    "someone",
                )
                .to_owned(),
                reward_title: reward
                    .and_then(|r| r.get("title"))
                    .and_then(Value::as_str)
                    .unwrap_or("Custom Reward")
                    .to_owned(),
                cost: reward
                    .and_then(|r| r.get("cost"))
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or(0),
                reward_id: pick_optional_non_empty(
                    reward.and_then(|r| r.get("id")).and_then(Value::as_str),
                    reward
                        .and_then(|r| r.get("reward_id"))
                        .and_then(Value::as_str),
                )
                .map(str::to_owned),
                redemption_id: pick_optional_non_empty(
                    event.get("id").and_then(Value::as_str),
                    event.get("redemption_id").and_then(Value::as_str),
                )
                .map(str::to_owned),
                user_input: pick_optional_non_empty(
                    event.get("user_input").and_then(Value::as_str),
                    event.get("user_input_text").and_then(Value::as_str),
                )
                .map(str::to_owned),
                status: pick_optional_non_empty(
                    event.get("status").and_then(Value::as_str),
                    event.get("redemption_status").and_then(Value::as_str),
                )
                .map(str::to_owned),
                is_update: sub_type.ends_with(".update"),
            }
        }
        "channel.poll.begin" | "channel.poll.progress" | "channel.poll.end" => {
            let phase = sub_type
                .split('.')
                .next_back()
                .unwrap_or("update")
                .to_owned();
            EventSubNoticeKind::PollLifecycle {
                title: event
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Untitled poll")
                    .to_owned(),
                phase,
                status: event
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                details: summarize_poll_details(event),
            }
        }
        "channel.prediction.begin"
        | "channel.prediction.progress"
        | "channel.prediction.lock"
        | "channel.prediction.end" => {
            let phase = sub_type
                .split('.')
                .next_back()
                .unwrap_or("update")
                .to_owned();
            EventSubNoticeKind::PredictionLifecycle {
                title: event
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Untitled prediction")
                    .to_owned(),
                phase,
                status: event
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                details: summarize_prediction_details(event),
            }
        }
        "automod.message.hold" => {
            let message = event.get("message");
            let sender = message.and_then(|m| m.get("sender"));
            let reason_text = event
                .get("reason")
                .and_then(|r| {
                    r.get("reason")
                        .and_then(Value::as_str)
                        .or_else(|| r.get("type").and_then(Value::as_str))
                })
                .map(str::to_owned)
                .or_else(|| event.get("reason").map(|r| r.to_string()));

            EventSubNoticeKind::AutoModMessageHold {
                message_id: pick_non_empty(
                    event.get("message_id").and_then(Value::as_str),
                    event.get("msg_id").and_then(Value::as_str),
                    "",
                )
                .to_owned(),
                sender_user_id: pick_non_empty(
                    event.get("user_id").and_then(Value::as_str),
                    sender
                        .and_then(|s| s.get("user_id"))
                        .and_then(Value::as_str),
                    "",
                )
                .to_owned(),
                sender_login: pick_non_empty(
                    event.get("user_login").and_then(Value::as_str),
                    sender
                        .and_then(|s| s.get("user_login"))
                        .and_then(Value::as_str),
                    "unknown",
                )
                .to_owned(),
                text: pick_non_empty(
                    message.and_then(|m| m.get("text")).and_then(Value::as_str),
                    event.get("text").and_then(Value::as_str),
                    "",
                )
                .to_owned(),
                reason: reason_text,
            }
        }
        "automod.message.update" => EventSubNoticeKind::AutoModMessageUpdate {
            message_id: pick_non_empty(
                event.get("message_id").and_then(Value::as_str),
                event.get("msg_id").and_then(Value::as_str),
                "",
            )
            .to_owned(),
            status: pick_non_empty(
                event.get("status").and_then(Value::as_str),
                event.get("action").and_then(Value::as_str),
                "UNKNOWN",
            )
            .to_owned(),
        },
        "channel.unban_request.create" => EventSubNoticeKind::UnbanRequestCreate {
            request_id: pick_non_empty(
                event.get("id").and_then(Value::as_str),
                event.get("unban_request_id").and_then(Value::as_str),
                "",
            )
            .to_owned(),
            user_id: pick_non_empty(
                event.get("user_id").and_then(Value::as_str),
                event.get("requester_user_id").and_then(Value::as_str),
                "",
            )
            .to_owned(),
            user_login: pick_non_empty(
                event.get("user_login").and_then(Value::as_str),
                event.get("requester_user_login").and_then(Value::as_str),
                "unknown",
            )
            .to_owned(),
            text: pick_optional_non_empty(
                event.get("text").and_then(Value::as_str),
                event.get("message").and_then(Value::as_str),
            )
            .map(str::to_owned),
            created_at: pick_optional_non_empty(
                event.get("created_at").and_then(Value::as_str),
                event.get("requested_at").and_then(Value::as_str),
            )
            .map(str::to_owned),
        },
        "channel.unban_request.resolve" => EventSubNoticeKind::UnbanRequestResolve {
            request_id: pick_non_empty(
                event.get("id").and_then(Value::as_str),
                event.get("unban_request_id").and_then(Value::as_str),
                "",
            )
            .to_owned(),
            status: pick_non_empty(
                event.get("status").and_then(Value::as_str),
                event.get("resolution_status").and_then(Value::as_str),
                "UNKNOWN",
            )
            .to_owned(),
        },
        "channel.ban" => EventSubNoticeKind::ChannelBan {
            user_login: pick_non_empty(
                event.get("user_login").and_then(Value::as_str),
                event.get("user_name").and_then(Value::as_str),
                "unknown",
            )
            .to_owned(),
            reason: pick_optional_non_empty(
                event.get("reason").and_then(Value::as_str),
                event.get("moderator_message").and_then(Value::as_str),
            )
            .map(str::to_owned),
            ends_at: pick_optional_non_empty(
                event.get("ends_at").and_then(Value::as_str),
                event.get("expires_at").and_then(Value::as_str),
            )
            .map(str::to_owned),
        },
        "channel.unban" => EventSubNoticeKind::ChannelUnban {
            user_login: pick_non_empty(
                event.get("user_login").and_then(Value::as_str),
                event.get("user_name").and_then(Value::as_str),
                "unknown",
            )
            .to_owned(),
        },
        "channel.suspicious_user.message" => EventSubNoticeKind::SuspiciousUserMessage {
            user_id: pick_non_empty(event.get("user_id").and_then(Value::as_str), Some(""), "")
                .to_owned(),
            user_login: pick_non_empty(
                event.get("user_login").and_then(Value::as_str),
                event.get("user_name").and_then(Value::as_str),
                "unknown",
            )
            .to_owned(),
            user_name: pick_non_empty(
                event.get("user_name").and_then(Value::as_str),
                event.get("user_login").and_then(Value::as_str),
                "unknown",
            )
            .to_owned(),
            low_trust_status: pick_non_empty(
                event.get("low_trust_status").and_then(Value::as_str),
                Some(""),
                "none",
            )
            .to_owned(),
            ban_evasion_evaluation: pick_optional_non_empty(
                event.get("ban_evasion_evaluation").and_then(Value::as_str),
                event.get("banEvasionEvaluation").and_then(Value::as_str),
            )
            .map(str::to_owned),
            shared_ban_channel_ids: event
                .get("shared_ban_channel_ids")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            types: event
                .get("types")
                .and_then(Value::as_array)
                .map(|types| {
                    types
                        .iter()
                        .filter_map(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            text: event
                .get("message")
                .and_then(|m| m.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        },
        "channel.suspicious_user.update" => EventSubNoticeKind::SuspiciousUserUpdate {
            user_id: pick_non_empty(event.get("user_id").and_then(Value::as_str), Some(""), "")
                .to_owned(),
            user_login: pick_non_empty(
                event.get("user_login").and_then(Value::as_str),
                event.get("user_name").and_then(Value::as_str),
                "unknown",
            )
            .to_owned(),
            user_name: pick_non_empty(
                event.get("user_name").and_then(Value::as_str),
                event.get("user_login").and_then(Value::as_str),
                "unknown",
            )
            .to_owned(),
            moderator_user_id: pick_non_empty(
                event.get("moderator_user_id").and_then(Value::as_str),
                Some(""),
                "",
            )
            .to_owned(),
            moderator_login: pick_non_empty(
                event.get("moderator_user_login").and_then(Value::as_str),
                event.get("moderator_user_name").and_then(Value::as_str),
                "unknown",
            )
            .to_owned(),
            moderator_name: pick_non_empty(
                event.get("moderator_user_name").and_then(Value::as_str),
                event.get("moderator_user_login").and_then(Value::as_str),
                "unknown",
            )
            .to_owned(),
            low_trust_status: pick_non_empty(
                event.get("low_trust_status").and_then(Value::as_str),
                Some(""),
                "none",
            )
            .to_owned(),
        },
        "user.whisper.message" => EventSubNoticeKind::UserWhisperMessage {
            from_user_id: pick_non_empty(
                event.get("from_user_id").and_then(Value::as_str),
                Some(""),
                "",
            )
            .to_owned(),
            from_user_login: pick_non_empty(
                event.get("from_user_login").and_then(Value::as_str),
                event.get("from_user_name").and_then(Value::as_str),
                "unknown",
            )
            .to_owned(),
            from_user_name: pick_non_empty(
                event.get("from_user_name").and_then(Value::as_str),
                event.get("from_user_login").and_then(Value::as_str),
                "unknown",
            )
            .to_owned(),
            to_user_id: pick_non_empty(
                event.get("to_user_id").and_then(Value::as_str),
                Some(""),
                "",
            )
            .to_owned(),
            to_user_login: pick_non_empty(
                event.get("to_user_login").and_then(Value::as_str),
                event.get("to_user_name").and_then(Value::as_str),
                "unknown",
            )
            .to_owned(),
            to_user_name: pick_non_empty(
                event.get("to_user_name").and_then(Value::as_str),
                event.get("to_user_login").and_then(Value::as_str),
                "unknown",
            )
            .to_owned(),
            whisper_id: pick_non_empty(
                event.get("whisper_id").and_then(Value::as_str),
                event.get("id").and_then(Value::as_str),
                "",
            )
            .to_owned(),
            text: event
                .get("whisper")
                .and_then(|m| m.get("text"))
                .and_then(Value::as_str)
                .or_else(|| event.get("text").and_then(Value::as_str))
                .unwrap_or("")
                .to_owned(),
        },
        "channel.moderate" => {
            let action = parse_moderation_action(event);
            EventSubNoticeKind::ModerationAction {
                moderator_login: pick_non_empty(
                    event.get("moderator_user_login").and_then(Value::as_str),
                    event.get("moderator_user_name").and_then(Value::as_str),
                    "a moderator",
                )
                .to_owned(),
                action,
                target_login: pick_optional_non_empty(
                    event.get("user_login").and_then(Value::as_str),
                    event.get("target_user_login").and_then(Value::as_str),
                )
                .map(str::to_owned),
                target_message_id: parse_moderation_target_message_id(event),
                source_channel_login: pick_optional_non_empty(
                    event
                        .get("source_broadcaster_user_login")
                        .and_then(Value::as_str),
                    event
                        .get("source_broadcaster_user_name")
                        .and_then(Value::as_str),
                )
                .map(str::to_owned),
            }
        }
        "stream.online" => EventSubNoticeKind::StreamOnline,
        "stream.offline" => EventSubNoticeKind::StreamOffline,
        _ => return None,
    };

    Some(EventSubNotice {
        event_id,
        broadcaster_id,
        broadcaster_login,
        kind,
    })
}

fn pick_non_empty<'a>(a: Option<&'a str>, b: Option<&'a str>, fallback: &'a str) -> &'a str {
    pick_optional_non_empty(a, b).unwrap_or(fallback)
}

fn pick_optional_non_empty<'a>(a: Option<&'a str>, b: Option<&'a str>) -> Option<&'a str> {
    a.filter(|v| !v.trim().is_empty())
        .or_else(|| b.filter(|v| !v.trim().is_empty()))
}

fn normalize_tier(raw: Option<&str>) -> String {
    match raw.unwrap_or("") {
        "1000" => "Tier 1".to_owned(),
        "2000" => "Tier 2".to_owned(),
        "3000" => "Tier 3".to_owned(),
        "Prime" | "prime" => "Prime".to_owned(),
        other if !other.trim().is_empty() => other.to_owned(),
        _ => "Unknown tier".to_owned(),
    }
}

fn parse_moderation_action(event: &Value) -> String {
    let Some(action) = event.get("action") else {
        return "updated moderation state".to_owned();
    };

    if let Some(action_str) = action.as_str() {
        let trimmed = action_str.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }

    if let Some(action_obj) = action.as_object() {
        if let Some(kind) = action_obj
            .get("type")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            return kind.to_owned();
        }

        for key in [
            "ban",
            "unban",
            "timeout",
            "untimeout",
            "warn",
            "delete",
            "clear",
            "slow",
            "slowoff",
            "followers",
            "followersoff",
            "subscribers",
            "subscribersoff",
            "emoteonly",
            "emoteonlyoff",
            "uniquechat",
            "uniquechatoff",
            "raid",
            "unraid",
            "mod",
            "unmod",
            "vip",
            "unvip",
            "monitor",
            "unmonitor",
            "restrict",
            "unrestrict",
        ] {
            if let Some(value) = action_obj.get(key) {
                if key == "timeout" {
                    let seconds = value
                        .get("duration_seconds")
                        .and_then(Value::as_u64)
                        .or_else(|| value.get("duration").and_then(Value::as_u64));
                    if let Some(seconds) = seconds {
                        return format!("timeout_{seconds}s");
                    }
                }
                if key == "slow" {
                    let seconds = value
                        .get("wait_time_seconds")
                        .and_then(Value::as_u64)
                        .or_else(|| value.get("duration_seconds").and_then(Value::as_u64));
                    if let Some(seconds) = seconds {
                        return format!("slow_{seconds}s");
                    }
                }
                if key == "followers" {
                    let minutes = value
                        .get("follow_duration_minutes")
                        .and_then(Value::as_u64)
                        .or_else(|| value.get("duration_minutes").and_then(Value::as_u64));
                    if let Some(minutes) = minutes {
                        return format!("followers_{minutes}m");
                    }
                }
                return key.to_owned();
            }
        }

        if let Some(first_key) = action_obj.keys().next() {
            if !first_key.trim().is_empty() {
                return first_key.to_owned();
            }
        }
    }

    "updated moderation state".to_owned()
}

fn parse_moderation_target_message_id(event: &Value) -> Option<String> {
    if let Some(message_id) = event
        .get("message_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(message_id.to_owned());
    }

    event
        .get("action")
        .and_then(|value| find_nested_string_field(value, "message_id"))
}

fn find_nested_string_field(value: &Value, field_name: &str) -> Option<String> {
    match value {
        Value::Object(map) => {
            if let Some(found) = map
                .get(field_name)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(found.to_owned());
            }
            for nested in map.values() {
                if let Some(found) = find_nested_string_field(nested, field_name) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => {
            for nested in items {
                if let Some(found) = find_nested_string_field(nested, field_name) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

fn summarize_poll_details(event: &Value) -> Option<String> {
    let choices = event.get("choices")?.as_array()?;
    if choices.is_empty() {
        return None;
    }

    let mut rows: Vec<(String, u64)> = choices
        .iter()
        .filter_map(|choice| {
            let title = choice
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())?
                .to_owned();
            let votes = choice
                .get("votes")
                .and_then(Value::as_u64)
                .or_else(|| {
                    let points = choice
                        .get("channel_points_votes")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let bits = choice
                        .get("bits_votes")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    Some(points.saturating_add(bits))
                })
                .unwrap_or(0);
            Some((title, votes))
        })
        .collect();

    if rows.is_empty() {
        return None;
    }

    rows.sort_by(|(a_title, a_votes), (b_title, b_votes)| {
        b_votes.cmp(a_votes).then_with(|| a_title.cmp(b_title))
    });
    let total: u64 = rows.iter().map(|(_, votes)| *votes).sum();

    let summary = rows
        .into_iter()
        .take(3)
        .map(|(title, votes)| {
            if total > 0 {
                let pct = ((votes as f64 / total as f64) * 100.0).round() as u64;
                format!("{title} {pct}% ({votes})")
            } else {
                format!("{title} ({votes})")
            }
        })
        .collect::<Vec<_>>()
        .join(" | ");

    if summary.is_empty() {
        None
    } else {
        Some(format!("Top: {summary}"))
    }
}

fn summarize_prediction_details(event: &Value) -> Option<String> {
    let outcomes = event.get("outcomes")?.as_array()?;
    if outcomes.is_empty() {
        return None;
    }

    let winning_id = event
        .get("winning_outcome_id")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let mut rows: Vec<(String, u64, u64, bool)> = outcomes
        .iter()
        .filter_map(|outcome| {
            let title = outcome
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())?
                .to_owned();
            let points = outcome
                .get("channel_points")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let users = outcome.get("users").and_then(Value::as_u64).unwrap_or(0);
            let is_winner = winning_id
                .as_deref()
                .and_then(|wid| {
                    outcome
                        .get("id")
                        .and_then(Value::as_str)
                        .map(|id| id == wid)
                })
                .unwrap_or(false);
            Some((title, points, users, is_winner))
        })
        .collect();

    if rows.is_empty() {
        return None;
    }

    rows.sort_by(|(a_title, a_points, _, _), (b_title, b_points, _, _)| {
        b_points.cmp(a_points).then_with(|| a_title.cmp(b_title))
    });
    let total_points: u64 = rows.iter().map(|(_, points, _, _)| *points).sum();

    let summary = rows
        .into_iter()
        .take(3)
        .map(|(title, points, users, is_winner)| {
            let winner = if is_winner { " [winner]" } else { "" };
            if total_points > 0 {
                let pct = ((points as f64 / total_points as f64) * 100.0).round() as u64;
                format!(
                    "{title} {pct}% ({} pts, {} users){winner}",
                    compact_u64(points),
                    users
                )
            } else {
                format!(
                    "{title} ({} pts, {} users){winner}",
                    compact_u64(points),
                    users
                )
            }
        })
        .collect::<Vec<_>>()
        .join(" | ");

    if summary.is_empty() {
        None
    } else {
        Some(format!("Top: {summary}"))
    }
}

fn compact_u64(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{parse_notice, subscription_specs, EventSubNoticeKind};

    #[test]
    fn moderator_scoped_subscriptions_include_ban_and_moderate_topics() {
        let specs = subscription_specs("123", "456");
        let mut kinds: Vec<&str> = specs.iter().map(|s| s.kind).collect();
        kinds.sort_unstable();

        assert!(kinds.contains(&"channel.ban"));
        assert!(kinds.contains(&"channel.unban"));
        assert!(kinds.contains(&"channel.moderate"));
        assert!(kinds.contains(&"channel.chat.user_message_hold"));
        assert!(kinds.contains(&"channel.chat.user_message_update"));
        assert!(kinds.contains(&"channel.suspicious_user.message"));
        assert!(kinds.contains(&"channel.suspicious_user.update"));
    }

    #[test]
    fn parse_channel_ban_notice_extracts_user_and_reason() {
        let payload = json!({
            "payload": {
                "event": {
                    "broadcaster_user_id": "123",
                    "broadcaster_user_login": "streamer",
                    "user_login": "troublemaker",
                    "reason": "spam",
                    "ends_at": "2026-03-31T12:00:00Z"
                }
            }
        });

        let notice = parse_notice(&payload, "channel.ban").expect("channel.ban parsed");
        match notice.kind {
            EventSubNoticeKind::ChannelBan {
                user_login,
                reason,
                ends_at,
            } => {
                assert_eq!(user_login, "troublemaker");
                assert_eq!(reason.as_deref(), Some("spam"));
                assert_eq!(ends_at.as_deref(), Some("2026-03-31T12:00:00Z"));
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parse_notice_extracts_event_id_from_metadata() {
        let payload = json!({
            "metadata": {
                "message_id": "evt-123"
            },
            "payload": {
                "event": {
                    "broadcaster_user_id": "123",
                    "broadcaster_user_login": "streamer",
                    "user_login": "troublemaker"
                }
            }
        });

        let notice = parse_notice(&payload, "channel.ban").expect("channel.ban parsed");
        assert_eq!(notice.event_id.as_deref(), Some("evt-123"));
    }

    #[test]
    fn parse_channel_moderate_notice_extracts_action_and_target() {
        let payload = json!({
            "payload": {
                "event": {
                    "broadcaster_user_id": "123",
                    "broadcaster_user_login": "streamer",
                    "moderator_user_login": "mod_jane",
                    "target_user_login": "viewer123",
                    "action": {
                        "type": "warn"
                    }
                }
            }
        });

        let notice = parse_notice(&payload, "channel.moderate").expect("channel.moderate parsed");
        match notice.kind {
            EventSubNoticeKind::ModerationAction {
                moderator_login,
                action,
                target_login,
                target_message_id,
                source_channel_login,
            } => {
                assert_eq!(moderator_login, "mod_jane");
                assert_eq!(action, "warn");
                assert_eq!(target_login.as_deref(), Some("viewer123"));
                assert_eq!(target_message_id, None);
                assert_eq!(source_channel_login, None);
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parse_channel_moderate_delete_extracts_message_id() {
        let payload = json!({
            "payload": {
                "event": {
                    "broadcaster_user_id": "123",
                    "broadcaster_user_login": "streamer",
                    "moderator_user_login": "mod_jane",
                    "target_user_login": "viewer123",
                    "action": {
                        "delete": {
                            "message_id": "abc-123"
                        }
                    }
                }
            }
        });

        let notice = parse_notice(&payload, "channel.moderate").expect("channel.moderate parsed");
        match notice.kind {
            EventSubNoticeKind::ModerationAction {
                action,
                target_message_id,
                source_channel_login,
                ..
            } => {
                assert_eq!(action, "delete");
                assert_eq!(target_message_id.as_deref(), Some("abc-123"));
                assert_eq!(source_channel_login, None);
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parse_channel_moderate_extracts_shared_chat_source_channel() {
        let payload = json!({
            "payload": {
                "event": {
                    "broadcaster_user_id": "123",
                    "broadcaster_user_login": "streamer",
                    "moderator_user_login": "mod_jane",
                    "target_user_login": "viewer123",
                    "source_broadcaster_user_login": "partner_stream",
                    "action": {
                        "type": "ban"
                    }
                }
            }
        });

        let notice = parse_notice(&payload, "channel.moderate").expect("channel.moderate parsed");
        match notice.kind {
            EventSubNoticeKind::ModerationAction {
                action,
                source_channel_login,
                ..
            } => {
                assert_eq!(action, "ban");
                assert_eq!(source_channel_login.as_deref(), Some("partner_stream"));
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parse_channel_chat_user_message_update_extracts_status() {
        let payload = json!({
            "payload": {
                "event": {
                    "broadcaster_user_id": "123",
                    "broadcaster_user_login": "streamer",
                    "user_id": "456",
                    "user_login": "viewer",
                    "user_name": "Viewer",
                    "message_id": "msg-1",
                    "status": "approved",
                    "message": {
                        "text": "hello"
                    }
                }
            }
        });

        let notice = parse_notice(&payload, "channel.chat.user_message_update")
            .expect("channel.chat.user_message_update parsed");
        match notice.kind {
            EventSubNoticeKind::ChannelChatUserMessageUpdate {
                message_id,
                user_login,
                status,
                ..
            } => {
                assert_eq!(message_id, "msg-1");
                assert_eq!(user_login, "viewer");
                assert_eq!(status, "approved");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parse_suspicious_user_message_extracts_details() {
        let payload = json!({
            "payload": {
                "event": {
                    "broadcaster_user_id": "123",
                    "broadcaster_user_login": "streamer",
                    "user_id": "456",
                    "user_login": "viewer",
                    "user_name": "Viewer",
                    "low_trust_status": "restricted",
                    "ban_evasion_evaluation": "likely",
                    "shared_ban_channel_ids": ["1", "2"],
                    "types": ["ban_evader_detector"],
                    "message": {
                        "text": "hello"
                    }
                }
            }
        });

        let notice = parse_notice(&payload, "channel.suspicious_user.message")
            .expect("channel.suspicious_user.message parsed");
        match notice.kind {
            EventSubNoticeKind::SuspiciousUserMessage {
                low_trust_status,
                ban_evasion_evaluation,
                shared_ban_channel_ids,
                types,
                text,
                ..
            } => {
                assert_eq!(low_trust_status, "restricted");
                assert_eq!(ban_evasion_evaluation.as_deref(), Some("likely"));
                assert_eq!(shared_ban_channel_ids, vec!["1".to_owned(), "2".to_owned()]);
                assert_eq!(types, vec!["ban_evader_detector".to_owned()]);
                assert_eq!(text, "hello");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parse_channel_moderate_timeout_extracts_duration_action() {
        let payload = json!({
            "payload": {
                "event": {
                    "broadcaster_user_id": "123",
                    "broadcaster_user_login": "streamer",
                    "moderator_user_login": "mod_jane",
                    "target_user_login": "viewer123",
                    "action": {
                        "timeout": {
                            "duration_seconds": 600
                        }
                    }
                }
            }
        });

        let notice = parse_notice(&payload, "channel.moderate").expect("channel.moderate parsed");
        match notice.kind {
            EventSubNoticeKind::ModerationAction { action, .. } => {
                assert_eq!(action, "timeout_600s");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parse_channel_moderate_slow_extracts_wait_seconds() {
        let payload = json!({
            "payload": {
                "event": {
                    "broadcaster_user_id": "123",
                    "broadcaster_user_login": "streamer",
                    "moderator_user_login": "mod_jane",
                    "action": {
                        "slow": {
                            "wait_time_seconds": 15
                        }
                    }
                }
            }
        });

        let notice = parse_notice(&payload, "channel.moderate").expect("channel.moderate parsed");
        match notice.kind {
            EventSubNoticeKind::ModerationAction { action, .. } => {
                assert_eq!(action, "slow_15s");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parse_poll_progress_includes_choice_summary_details() {
        let payload = json!({
            "payload": {
                "event": {
                    "broadcaster_user_id": "123",
                    "title": "Best snack?",
                    "status": "ACTIVE",
                    "choices": [
                        {"title": "Pizza", "votes": 120},
                        {"title": "Burgers", "votes": 80}
                    ]
                }
            }
        });

        let notice =
            parse_notice(&payload, "channel.poll.progress").expect("channel.poll.progress parsed");
        match notice.kind {
            EventSubNoticeKind::PollLifecycle { details, .. } => {
                let details = details.expect("poll details");
                assert!(details.contains("Pizza"));
                assert!(details.contains("Burgers"));
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn parse_prediction_progress_includes_outcome_summary_details() {
        let payload = json!({
            "payload": {
                "event": {
                    "broadcaster_user_id": "123",
                    "title": "Will boss die?",
                    "status": "ACTIVE",
                    "outcomes": [
                        {"id": "o1", "title": "Yes", "channel_points": 1500, "users": 21},
                        {"id": "o2", "title": "No", "channel_points": 500, "users": 8}
                    ]
                }
            }
        });

        let notice = parse_notice(&payload, "channel.prediction.progress")
            .expect("channel.prediction.progress parsed");
        match notice.kind {
            EventSubNoticeKind::PredictionLifecycle { details, .. } => {
                let details = details.expect("prediction details");
                assert!(details.contains("Yes"));
                assert!(details.contains("No"));
                assert!(details.contains("pts"));
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }
}

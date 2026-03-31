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
    ChannelPointsRedemption {
        user_login: String,
        reward_title: String,
        cost: u32,
        reward_id: Option<String>,
        redemption_id: Option<String>,
        user_input: Option<String>,
        status: Option<String>,
    },
    PollLifecycle {
        title: String,
        phase: String,
        status: Option<String>,
    },
    PredictionLifecycle {
        title: String,
        phase: String,
        status: Option<String>,
    },
    StreamOnline,
    StreamOffline,
}

#[derive(Debug, Clone)]
pub struct EventSubNotice {
    pub broadcaster_id: String,
    pub broadcaster_login: Option<String>,
    pub kind: EventSubNoticeKind,
}

#[derive(Debug, Clone)]
pub enum EventSubEvent {
    Connected {
        resumed: bool,
    },
    Reconnecting {
        attempt: u32,
    },
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
}

impl EventSubSession {
    pub fn new(event_tx: mpsc::Sender<EventSubEvent>, cmd_rx: mpsc::Receiver<EventSubCommand>) -> Self {
        Self {
            event_tx,
            cmd_rx,
            auth: None,
            watched_broadcasters: HashSet::new(),
            http: reqwest::Client::new(),
            resumed_once: false,
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
                    self.emit(EventSubEvent::Error(format!("EventSub connection error: {e}")))
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
                        spec.kind,
                        broadcaster_id
                    );
                }
                Ok(r) => {
                    let status = r.status();
                    let body = r.text().await.unwrap_or_default();
                    warn!(
                        "EventSub subscribe failed {} for {}: HTTP {} - {}",
                        spec.kind,
                        broadcaster_id,
                        status,
                        body
                    );
                }
                Err(e) => {
                    warn!(
                        "EventSub subscribe request failed {} for {}: {}",
                        spec.kind,
                        broadcaster_id,
                        e
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
    }

    out
}

fn parse_notice(root: &Value, sub_type: &str) -> Option<EventSubNotice> {
    let payload = root.get("payload")?;
    let event = payload.get("event")?;
    let subscription = payload.get("subscription");

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
        .or_else(|| event.get("to_broadcaster_user_login").and_then(Value::as_str))
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
        "channel.channel_points_custom_reward_redemption.add" => {
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
                status: event.get("status").and_then(Value::as_str).map(str::to_owned),
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
                status: event.get("status").and_then(Value::as_str).map(str::to_owned),
            }
        }
        "stream.online" => EventSubNoticeKind::StreamOnline,
        "stream.offline" => EventSubNoticeKind::StreamOffline,
        _ => return None,
    };

    Some(EventSubNotice {
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

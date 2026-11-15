use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::connect_async;
use tracing::{debug, info, warn};

mod subscriptions;
use subscriptions::SubscriptionWorker;

const EVENTSUB_WS_URL: &str = "wss://eventsub.wss.twitch.tv/ws";
const EVENTSUB_SUBSCRIPTIONS_URL: &str = "https://api.twitch.tv/helix/eventsub/subscriptions";
const RECONNECT_BACKOFF_SECS: &[u64] = &[1, 2, 5, 10, 20, 30];
const EVENTSUB_WELCOME_TIMEOUT: Duration = Duration::from_secs(10);
// Twitch sends an idle keepalive at the advertised interval. Allow delivery and
// scheduler jitter before declaring that connection lost.
const EVENTSUB_KEEPALIVE_GRACE: Duration = Duration::from_secs(2);
const EVENTSUB_HEALTHY_SESSION: Duration = Duration::from_secs(30);

/// EventSub event deduplication cache size (number of recent event IDs to track).
const EVENT_DEDUP_CACHE_SIZE: usize = 10000;

#[derive(Debug, Clone)]
pub enum EventSubCommand {
    SetAuth {
        token: String,
        client_id: String,
        user_id: String,
        scopes: Vec<String>,
    },
    ClearAuth,
    WatchChannel {
        broadcaster_id: String,
        can_moderate: bool,
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
    HypeTrainLifecycle {
        train_id: String,
        phase: String,
        level: u32,
        progress: u64,
        goal: u64,
        total: u64,
        last_contribution_login: Option<String>,
        top_contribution_login: Option<String>,
        top_contribution_type: Option<String>,
        top_contribution_total: Option<u64>,
        started_at: Option<String>,
        ends_at: Option<String>,
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
        reason: Option<String>,
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
    /// Service event time, retained for moderation cutoffs.
    pub timestamp: Option<chrono::DateTime<chrono::Utc>>,
    pub moderation_target_user_id: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct EventSubAuth {
    token: String,
    client_id: String,
    user_id: String,
    scopes: HashSet<String>,
}

impl EventSubAuth {
    fn has_scope(&self, scope: &str) -> bool {
        self.scopes.contains(&scope.to_ascii_lowercase())
    }

    fn has_any_scope(&self, scopes: &[&str]) -> bool {
        scopes.iter().any(|scope| self.has_scope(scope))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct WatchedBroadcaster {
    can_moderate: bool,
}

pub struct EventSubSession {
    event_tx: mpsc::Sender<EventSubEvent>,
    cmd_rx: mpsc::Receiver<EventSubCommand>,
    auth: Option<EventSubAuth>,
    watched_broadcasters: HashMap<String, WatchedBroadcaster>,
    http: reqwest::Client,
    subscriptions_url: String,
    resumed_once: bool,
    reconnect_attempt: u32,
    /// Deduplication cache: tracks recent event IDs to prevent duplicate processing.
    seen_event_ids: RecentEventIds,
}

/// Bounded event ID history, ordered by first receipt.
struct RecentEventIds {
    items: HashSet<String>,
    order: VecDeque<String>,
    capacity: usize,
}

impl RecentEventIds {
    fn new(capacity: usize) -> Self {
        Self {
            items: HashSet::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn contains(&self, key: &str) -> bool {
        self.items.contains(key)
    }

    fn insert(&mut self, key: String) {
        if self.capacity == 0 || self.items.contains(&key) {
            return;
        }
        if self.items.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.items.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.items.insert(key);
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
            watched_broadcasters: HashMap::new(),
            http: reqwest::Client::new(),
            subscriptions_url: EVENTSUB_SUBSCRIPTIONS_URL.to_owned(),
            resumed_once: false,
            reconnect_attempt: 0,
            seen_event_ids: RecentEventIds::new(EVENT_DEDUP_CACHE_SIZE),
        }
    }

    async fn emit(&self, evt: EventSubEvent) {
        let _ = self.event_tx.send(evt).await;
    }

    pub async fn run(mut self) {
        let mut connect_url = EVENTSUB_WS_URL.to_owned();

        loop {
            self.emit(EventSubEvent::Reconnecting {
                attempt: self.reconnect_attempt,
            })
            .await;

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
                        self.reconnect_attempt = 0;
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
                .get(self.reconnect_attempt as usize)
                .copied()
                .unwrap_or(*RECONNECT_BACKOFF_SECS.last().unwrap_or(&30));
            match self.wait_for_reconnect(Duration::from_secs(delay)).await {
                EventSubConnectOutcome::Stop => return,
                EventSubConnectOutcome::Reconnect { immediate, .. } => {
                    if immediate {
                        self.reconnect_attempt = 0;
                        connect_url = EVENTSUB_WS_URL.to_owned();
                    } else {
                        self.reconnect_attempt = self.reconnect_attempt.saturating_add(1);
                    }
                }
            }
        }
    }

    async fn wait_for_reconnect(&mut self, delay: Duration) -> EventSubConnectOutcome {
        let retry = tokio::time::sleep(delay);
        tokio::pin!(retry);
        loop {
            tokio::select! {
                biased;
                command = self.cmd_rx.recv() => {
                    let Some(command) = command else {
                        return EventSubConnectOutcome::Stop;
                    };
                    if self.apply_command(command) == CommandEffect::AuthChanged {
                        return EventSubConnectOutcome::Reconnect {
                            reconnect_url: None,
                            immediate: true,
                        };
                    }
                }
                _ = &mut retry => {
                    return EventSubConnectOutcome::Reconnect {
                        reconnect_url: None,
                        immediate: false,
                    };
                }
            }
        }
    }

    fn apply_command(&mut self, command: EventSubCommand) -> CommandEffect {
        match command {
            EventSubCommand::SetAuth {
                token,
                client_id,
                user_id,
                scopes,
            } => {
                let token = token.trim();
                let bare = token
                    .strip_prefix("oauth:")
                    .unwrap_or(token)
                    .trim()
                    .to_owned();
                let next_auth = if bare.is_empty()
                    || client_id.trim().is_empty()
                    || user_id.trim().is_empty()
                {
                    None
                } else {
                    Some(EventSubAuth {
                        token: bare,
                        client_id: client_id.trim().to_owned(),
                        user_id: user_id.trim().to_owned(),
                        scopes: scopes
                            .into_iter()
                            .map(|scope| scope.trim().to_ascii_lowercase())
                            .filter(|scope| !scope.is_empty())
                            .collect(),
                    })
                };
                if self.auth != next_auth {
                    self.auth = next_auth;
                    CommandEffect::AuthChanged
                } else {
                    CommandEffect::Unchanged
                }
            }
            EventSubCommand::ClearAuth => {
                if self.auth.take().is_some() {
                    CommandEffect::AuthChanged
                } else {
                    CommandEffect::Unchanged
                }
            }
            EventSubCommand::WatchChannel {
                broadcaster_id,
                can_moderate,
            } => {
                let bid = broadcaster_id.trim();
                if bid.is_empty() {
                    return CommandEffect::Unchanged;
                }
                self.watched_broadcasters
                    .insert(bid.to_owned(), WatchedBroadcaster { can_moderate });
                CommandEffect::SubscriptionsChanged
            }
            EventSubCommand::UnwatchChannel { broadcaster_id } => {
                if self
                    .watched_broadcasters
                    .remove(broadcaster_id.trim())
                    .is_some()
                {
                    CommandEffect::SubscriptionsChanged
                } else {
                    CommandEffect::Unchanged
                }
            }
        }
    }

    async fn connect_once(&mut self, url: &str) -> Result<EventSubConnectOutcome, String> {
        info!("Connecting EventSub websocket: {url}");
        let connecting = tokio::time::timeout(Duration::from_secs(10), connect_async(url));
        tokio::pin!(connecting);
        let (ws, _) = loop {
            tokio::select! {
                biased;
                command = self.cmd_rx.recv() => {
                    let Some(command) = command else {
                        return Ok(EventSubConnectOutcome::Stop);
                    };
                    if self.apply_command(command) == CommandEffect::AuthChanged {
                        return Ok(EventSubConnectOutcome::Reconnect {
                            reconnect_url: None,
                            immediate: true,
                        });
                    }
                }
                result = &mut connecting => {
                    break result
                        .map_err(|_| "EventSub connection timed out".to_owned())?
                        .map_err(|error| format!("connect failed: {error}"))?;
                }
            }
        };
        let (mut sink, mut stream) = ws.split();

        let mut session_id: Option<String> = None;
        let mut subscriptions: Option<SubscriptionWorker> = None;
        let mut reconnect: Option<ReconnectAttempt> = None;
        let mut connected_since: Option<Instant> = None;
        let mut keepalive_timeout = EVENTSUB_WELCOME_TIMEOUT;
        let mut activity_deadline = Instant::now() + EVENTSUB_WELCOME_TIMEOUT;

        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(activity_deadline) => {
                    return Err(if session_id.is_none() {
                        "EventSub welcome timed out".to_owned()
                    } else {
                        "EventSub keepalive timed out".to_owned()
                    });
                }
                handoff = async { (&mut reconnect.as_mut().expect("pending reconnect").task).await }, if reconnect.is_some() => {
                    let (socket, welcome) = handoff.map_err(|error| format!("reconnect task failed: {error}"))??;
                    reconnect = None;
                    let sid = welcome.id;
                    keepalive_timeout = welcome.keepalive_timeout;
                    activity_deadline = welcome.received_at + keepalive_timeout + EVENTSUB_KEEPALIVE_GRACE;
                    connected_since.get_or_insert(welcome.received_at);
                    // Keep the logical HTTP worker through a socket transfer so
                    // recovery cannot overtake an accepted POST or DELETE.
                    if let Some(worker) = &subscriptions {
                        worker.resume(sid.clone(), self.desired_subscriptions());
                    } else {
                        subscriptions = self.auth.clone().map(|auth| SubscriptionWorker::start(
                            self.http.clone(),
                            self.subscriptions_url.clone(),
                            auth,
                            sid.clone(),
                            self.desired_subscriptions(),
                            true,
                        ));
                    }
                    // The new welcome confirms transfer before the old socket closes.
                    let (next_sink, next_stream) = socket.split();
                    sink = next_sink;
                    stream = next_stream;
                    session_id = Some(sid.clone());
                    self.emit(EventSubEvent::Connected { resumed: true }).await;
                    self.emit(EventSubEvent::BackfillRequested).await;
                }
                maybe_cmd = self.cmd_rx.recv() => {
                    let Some(cmd) = maybe_cmd else {
                        return Ok(EventSubConnectOutcome::Stop);
                    };
                    match self.apply_command(cmd) {
                        CommandEffect::AuthChanged => {
                            // Subscriptions belong to the account on this socket.
                            return Ok(EventSubConnectOutcome::Reconnect {
                                reconnect_url: None,
                                immediate: true,
                            });
                        }
                        CommandEffect::SubscriptionsChanged => {
                            if let Some(worker) = &subscriptions {
                                worker.replace(self.desired_subscriptions());
                            }
                        }
                        CommandEffect::Unchanged => {}
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

                            if session_id.is_some()
                                && matches!(message_type, "notification" | "session_keepalive" | "session_reconnect" | "revocation")
                                && parsed.get("payload").is_some_and(Value::is_object)
                            {
                                let received_at = Instant::now();
                                activity_deadline = received_at + keepalive_timeout + EVENTSUB_KEEPALIVE_GRACE;
                                if matches!(message_type, "notification" | "session_keepalive")
                                    && connected_since.is_some_and(|since| received_at.duration_since(since) >= EVENTSUB_HEALTHY_SESSION)
                                {
                                    self.reconnect_attempt = 0;
                                }
                            }

                            match message_type {
                                "session_welcome" => {
                                    let Some(welcome) = SessionWelcome::parse(&parsed) else {
                                        continue;
                                    };
                                    let sid = welcome.id;
                                    keepalive_timeout = welcome.keepalive_timeout;
                                    activity_deadline = welcome.received_at + keepalive_timeout + EVENTSUB_KEEPALIVE_GRACE;
                                    if session_id.as_deref() == Some(sid.as_str()) {
                                        continue;
                                    }

                                    session_id = Some(sid.clone());
                                    connected_since = Some(welcome.received_at);
                                    self.emit(EventSubEvent::Connected {
                                        resumed: self.resumed_once,
                                    }).await;
                                    self.emit(EventSubEvent::BackfillRequested).await;
                                    self.resumed_once = true;
                                    subscriptions = self.auth.clone().map(|auth| SubscriptionWorker::start(
                                        self.http.clone(),
                                        self.subscriptions_url.clone(),
                                        auth,
                                        sid,
                                        self.desired_subscriptions(),
                                        false,
                                    ));
                                }
                                "session_keepalive" => {
                                    // The valid envelope refreshed the activity deadline above.
                                }
                                "session_reconnect" => {
                                    let reconnect_url = parsed
                                        .get("payload")
                                        .and_then(|p| p.get("session"))
                                        .and_then(|s| s.get("reconnect_url"))
                                        .and_then(Value::as_str)
                                        .map(str::to_owned);

                                    if let Some(url) = reconnect_url {
                                        if reconnect.is_none() {
                                            reconnect = Some(ReconnectAttempt::start(url));
                                        }
                                    } else {
                                        return Ok(EventSubConnectOutcome::Reconnect {
                                            reconnect_url: None,
                                            immediate: true,
                                        });
                                    }
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
                                        if self.accepts_notice(sub_type, &notice.broadcaster_id) {
                                            self.emit(EventSubEvent::Notice(notice)).await;
                                        }
                                    }
                                }
                                other => {
                                    debug!("Unhandled EventSub message type: {other}");
                                }
                            }
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(payload))) => {
                            if let Err(e) = sink
                                .send(tokio_tungstenite::tungstenite::Message::Pong(payload))
                                .await
                            {
                                return Err(format!("websocket pong failed: {e}"));
                            }
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Close(frame))) => {
                            if let Some(frame) = frame {
                                warn!(
                                    "EventSub websocket closed by server: code={} reason={}",
                                    frame.code, frame.reason
                                );
                            } else {
                                warn!("EventSub websocket closed by server");
                            }
                            return Ok(EventSubConnectOutcome::Reconnect {
                                reconnect_url: None,
                                immediate: false,
                            });
                        }
                        Some(Ok(_)) => {
                            // Ignore binary/pong frames.
                        }
                    }
                }
            }
        }
    }

    fn accepts_notice(&self, sub_type: &str, broadcaster_id: &str) -> bool {
        self.auth.is_some()
            && (sub_type == "user.whisper.message"
                || self.watched_broadcasters.contains_key(broadcaster_id))
    }

    fn desired_subscriptions(&self) -> Vec<SubscriptionSpec> {
        let Some(auth) = self.auth.as_ref() else {
            return Vec::new();
        };
        let mut specs: Vec<_> = self
            .watched_broadcasters
            .iter()
            .flat_map(|(broadcaster_id, watched)| {
                subscription_specs(broadcaster_id, auth, watched.can_moderate)
            })
            .collect();
        if auth.has_any_scope(&["user:read:whispers", "user:manage:whispers"]) {
            specs.push(SubscriptionSpec {
                kind: "user.whisper.message",
                version: "1",
                condition: json!({ "user_id": auth.user_id }),
                estimated_cost: 0,
                priority: SubscriptionPriority::Critical,
                fallback: None,
            });
        }
        specs
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandEffect {
    Unchanged,
    AuthChanged,
    SubscriptionsChanged,
}

enum EventSubConnectOutcome {
    Reconnect {
        reconnect_url: Option<String>,
        immediate: bool,
    },
    Stop,
}

type EventSubSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct SessionWelcome {
    id: String,
    keepalive_timeout: Duration,
    received_at: Instant,
}

impl SessionWelcome {
    fn parse(message: &Value) -> Option<Self> {
        let session = message.get("payload")?.get("session")?;
        let id = session.get("id")?.as_str().filter(|id| !id.is_empty())?;
        let keepalive_timeout = session
            .get("keepalive_timeout_seconds")
            .and_then(Value::as_u64)
            .filter(|seconds| (10..=600).contains(seconds))
            .map(Duration::from_secs)
            .unwrap_or(EVENTSUB_WELCOME_TIMEOUT);
        Some(Self {
            id: id.to_owned(),
            keepalive_timeout,
            received_at: Instant::now(),
        })
    }
}

struct ReconnectAttempt {
    task: tokio::task::JoinHandle<Result<(EventSubSocket, SessionWelcome), String>>,
}

impl ReconnectAttempt {
    fn start(url: String) -> Self {
        let task = tokio::spawn(async move {
            tokio::time::timeout(EVENTSUB_WELCOME_TIMEOUT, async move {
                let (mut socket, _) = connect_async(&url)
                    .await
                    .map_err(|error| format!("reconnect failed: {error}"))?;
                while let Some(message) = socket.next().await {
                    match message.map_err(|error| format!("reconnect read failed: {error}"))? {
                        tokio_tungstenite::tungstenite::Message::Text(text) => {
                            let message: Value = serde_json::from_str(&text)
                                .map_err(|error| format!("invalid reconnect welcome: {error}"))?;
                            if message["metadata"]["message_type"].as_str()
                                == Some("session_welcome")
                            {
                                if let Some(welcome) = SessionWelcome::parse(&message) {
                                    return Ok((socket, welcome));
                                }
                            }
                        }
                        tokio_tungstenite::tungstenite::Message::Ping(payload) => {
                            socket
                                .send(tokio_tungstenite::tungstenite::Message::Pong(payload))
                                .await
                                .map_err(|error| format!("reconnect pong failed: {error}"))?;
                        }
                        tokio_tungstenite::tungstenite::Message::Close(_) => break,
                        _ => {}
                    }
                }
                Err("replacement socket closed before welcome".to_owned())
            })
            .await
            .map_err(|_| "replacement socket welcome timed out".to_owned())?
        });
        Self { task }
    }
}

impl Drop for ReconnectAttempt {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SubscriptionSpec {
    kind: &'static str,
    version: &'static str,
    condition: Value,
    estimated_cost: u32,
    priority: SubscriptionPriority,
    fallback: Option<SubscriptionFallback>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubscriptionPriority {
    Critical,
    Replaceable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubscriptionFallback {
    StreamStatusPolling,
    IrcUsernotice,
}

fn is_expected_authz_failure(status: StatusCode, body: &str) -> bool {
    status == StatusCode::FORBIDDEN
        && body
            .to_ascii_lowercase()
            .contains("subscription missing proper authorization")
}

fn has_any_scope(auth: &EventSubAuth, scopes: &[&str]) -> bool {
    auth.has_any_scope(scopes)
}

fn has_all_scope_groups(auth: &EventSubAuth, groups: &[&[&str]]) -> bool {
    groups.iter().all(|group| has_any_scope(auth, group))
}

fn estimated_cross_channel_cost(is_broadcaster: bool) -> u32 {
    if is_broadcaster {
        0
    } else {
        1
    }
}

fn should_subscribe_spec(spec: &SubscriptionSpec) -> bool {
    match spec.priority {
        SubscriptionPriority::Critical => true,
        SubscriptionPriority::Replaceable => match spec.fallback {
            // For joined channels, IRC USERNOTICE already carries raid notices.
            Some(SubscriptionFallback::IrcUsernotice) => false,
            // Cross-channel live state is already covered by profile refreshes and the live-feed poller.
            Some(SubscriptionFallback::StreamStatusPolling) => spec.estimated_cost == 0,
            None => spec.estimated_cost == 0,
        },
    }
}

fn subscription_specs(
    broadcaster_id: &str,
    auth: &EventSubAuth,
    can_moderate: bool,
) -> Vec<SubscriptionSpec> {
    let bid = broadcaster_id.trim();
    if bid.is_empty() {
        return Vec::new();
    }
    let uid = auth.user_id.trim();
    let is_broadcaster = bid == uid;
    let can_moderate = can_moderate || is_broadcaster;

    let mut candidates = vec![
        SubscriptionSpec {
            kind: "stream.online",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
            estimated_cost: estimated_cross_channel_cost(is_broadcaster),
            priority: SubscriptionPriority::Replaceable,
            fallback: Some(SubscriptionFallback::StreamStatusPolling),
        },
        SubscriptionSpec {
            kind: "stream.offline",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
            estimated_cost: estimated_cross_channel_cost(is_broadcaster),
            priority: SubscriptionPriority::Replaceable,
            fallback: Some(SubscriptionFallback::StreamStatusPolling),
        },
        SubscriptionSpec {
            kind: "channel.raid",
            version: "1",
            condition: json!({"to_broadcaster_user_id": bid}),
            estimated_cost: estimated_cross_channel_cost(is_broadcaster),
            priority: SubscriptionPriority::Replaceable,
            fallback: Some(SubscriptionFallback::IrcUsernotice),
        },
    ];

    if has_any_scope(auth, &["user:read:chat"]) {
        candidates.push(SubscriptionSpec {
            kind: "channel.chat.user_message_hold",
            version: "1",
            condition: json!({
                "broadcaster_user_id": bid,
                "user_id": uid,
            }),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
        candidates.push(SubscriptionSpec {
            kind: "channel.chat.user_message_update",
            version: "1",
            condition: json!({
                "broadcaster_user_id": bid,
                "user_id": uid,
            }),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
    }

    if is_broadcaster && has_any_scope(auth, &["channel:read:subscriptions"]) {
        candidates.push(SubscriptionSpec {
            kind: "channel.subscribe",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
        candidates.push(SubscriptionSpec {
            kind: "channel.subscription.gift",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
    }

    if is_broadcaster
        && has_any_scope(
            auth,
            &["channel:read:redemptions", "channel:manage:redemptions"],
        )
    {
        candidates.push(SubscriptionSpec {
            kind: "channel.channel_points_custom_reward_redemption.add",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
        candidates.push(SubscriptionSpec {
            kind: "channel.channel_points_custom_reward_redemption.update",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
    }

    if is_broadcaster && has_any_scope(auth, &["channel:read:polls", "channel:manage:polls"]) {
        candidates.push(SubscriptionSpec {
            kind: "channel.poll.begin",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
        candidates.push(SubscriptionSpec {
            kind: "channel.poll.progress",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
        candidates.push(SubscriptionSpec {
            kind: "channel.poll.end",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
    }

    if is_broadcaster
        && has_any_scope(
            auth,
            &["channel:read:predictions", "channel:manage:predictions"],
        )
    {
        candidates.push(SubscriptionSpec {
            kind: "channel.prediction.begin",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
        candidates.push(SubscriptionSpec {
            kind: "channel.prediction.progress",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
        candidates.push(SubscriptionSpec {
            kind: "channel.prediction.lock",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
        candidates.push(SubscriptionSpec {
            kind: "channel.prediction.end",
            version: "1",
            condition: json!({"broadcaster_user_id": bid}),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
    }

    if is_broadcaster && auth.has_scope("channel:read:hype_train") {
        for kind in [
            "channel.hype_train.begin",
            "channel.hype_train.progress",
            "channel.hype_train.end",
        ] {
            candidates.push(SubscriptionSpec {
                kind,
                version: "2",
                condition: json!({"broadcaster_user_id": bid}),
                estimated_cost: 0,
                priority: SubscriptionPriority::Critical,
                fallback: None,
            });
        }
    }

    if can_moderate && has_any_scope(auth, &["moderator:read:followers"]) {
        candidates.push(SubscriptionSpec {
            kind: "channel.follow",
            version: "2",
            condition: json!({
                "broadcaster_user_id": bid,
                "moderator_user_id": uid,
            }),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
    }

    if can_moderate && has_any_scope(auth, &["moderator:manage:automod"]) {
        candidates.push(SubscriptionSpec {
            kind: "automod.message.hold",
            version: "2",
            condition: json!({
                "broadcaster_user_id": bid,
                "moderator_user_id": uid,
            }),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
        candidates.push(SubscriptionSpec {
            kind: "automod.message.update",
            version: "1",
            condition: json!({
                "broadcaster_user_id": bid,
                "moderator_user_id": uid,
            }),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
    }

    if can_moderate
        && has_any_scope(
            auth,
            &[
                "moderator:read:unban_requests",
                "moderator:manage:unban_requests",
            ],
        )
    {
        candidates.push(SubscriptionSpec {
            kind: "channel.unban_request.create",
            version: "1",
            condition: json!({
                "broadcaster_user_id": bid,
                "moderator_user_id": uid,
            }),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
        candidates.push(SubscriptionSpec {
            kind: "channel.unban_request.resolve",
            version: "1",
            condition: json!({
                "broadcaster_user_id": bid,
                "moderator_user_id": uid,
            }),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
    }

    if can_moderate && has_any_scope(auth, &["channel:moderate"]) {
        candidates.push(SubscriptionSpec {
            kind: "channel.ban",
            version: "1",
            condition: json!({ "broadcaster_user_id": bid }),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
        candidates.push(SubscriptionSpec {
            kind: "channel.unban",
            version: "1",
            condition: json!({ "broadcaster_user_id": bid }),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
    }

    if can_moderate
        && has_all_scope_groups(
            auth,
            &[
                &[
                    "moderator:read:blocked_terms",
                    "moderator:manage:blocked_terms",
                ],
                &[
                    "moderator:read:chat_settings",
                    "moderator:manage:chat_settings",
                ],
                &[
                    "moderator:read:unban_requests",
                    "moderator:manage:unban_requests",
                ],
                &[
                    "moderator:read:banned_users",
                    "moderator:manage:banned_users",
                ],
                &[
                    "moderator:read:chat_messages",
                    "moderator:manage:chat_messages",
                ],
                &["moderator:read:warnings", "moderator:manage:warnings"],
                &["moderator:read:moderators"],
                &["moderator:read:vips"],
            ],
        )
    {
        candidates.push(SubscriptionSpec {
            kind: "channel.moderate",
            version: "2",
            condition: json!({
                "broadcaster_user_id": bid,
                "moderator_user_id": uid,
            }),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
    }

    if can_moderate && has_any_scope(auth, &["moderator:read:suspicious_users"]) {
        candidates.push(SubscriptionSpec {
            kind: "channel.suspicious_user.message",
            version: "1",
            condition: json!({
                "broadcaster_user_id": bid,
                "moderator_user_id": uid,
            }),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
        candidates.push(SubscriptionSpec {
            kind: "channel.suspicious_user.update",
            version: "1",
            condition: json!({
                "broadcaster_user_id": bid,
                "moderator_user_id": uid,
            }),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        });
    }

    candidates
        .into_iter()
        .filter(should_subscribe_spec)
        .collect()
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
        "channel.hype_train.begin" | "channel.hype_train.progress" | "channel.hype_train.end" => {
            let top = event
                .get("top_contributions")
                .and_then(Value::as_array)
                .and_then(|contributions| {
                    contributions.iter().max_by_key(|contribution| {
                        contribution
                            .get("total")
                            .and_then(Value::as_u64)
                            .unwrap_or(0)
                    })
                });
            let contribution_login = |contribution: &Value| {
                pick_optional_non_empty(
                    contribution.get("user_login").and_then(Value::as_str),
                    contribution.get("user_name").and_then(Value::as_str),
                )
                .map(str::to_owned)
            };
            EventSubNoticeKind::HypeTrainLifecycle {
                train_id: event.get("id").and_then(Value::as_str)?.to_owned(),
                phase: sub_type
                    .split('.')
                    .next_back()
                    .unwrap_or("progress")
                    .to_owned(),
                level: event
                    .get("level")
                    .and_then(Value::as_u64)
                    .and_then(|level| u32::try_from(level).ok())
                    .unwrap_or(0),
                progress: event.get("progress").and_then(Value::as_u64).unwrap_or(0),
                goal: event.get("goal").and_then(Value::as_u64).unwrap_or(0),
                total: event.get("total").and_then(Value::as_u64).unwrap_or(0),
                last_contribution_login: event
                    .get("last_contribution")
                    .and_then(contribution_login),
                top_contribution_login: top.and_then(contribution_login),
                top_contribution_type: top
                    .and_then(|contribution| contribution.get("type"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                top_contribution_total: top
                    .and_then(|contribution| contribution.get("total"))
                    .and_then(Value::as_u64),
                started_at: event
                    .get("started_at")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                ends_at: pick_optional_non_empty(
                    event.get("expires_at").and_then(Value::as_str),
                    event.get("ended_at").and_then(Value::as_str),
                )
                .map(str::to_owned),
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
            let action = parse_moderation_action(
                event,
                root.get("metadata")
                    .and_then(|metadata| metadata.get("message_timestamp"))
                    .and_then(Value::as_str),
            );
            EventSubNoticeKind::ModerationAction {
                moderator_login: pick_non_empty(
                    event.get("moderator_user_login").and_then(Value::as_str),
                    event.get("moderator_user_name").and_then(Value::as_str),
                    "a moderator",
                )
                .to_owned(),
                action,
                target_login: moderation_action_payload(event)
                    .and_then(|payload| find_nested_string_field(payload, "user_login"))
                    .or_else(|| {
                        pick_optional_non_empty(
                            event.get("user_login").and_then(Value::as_str),
                            event.get("target_user_login").and_then(Value::as_str),
                        )
                        .map(str::to_owned)
                    }),
                target_message_id: parse_moderation_target_message_id(event),
                reason: moderation_action_payload(event)
                    .and_then(|payload| payload.get("reason"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|reason| !reason.is_empty())
                    .map(str::to_owned),
                source_channel_login: pick_optional_non_empty(
                    event
                        .get("source_broadcaster_user_login")
                        .and_then(Value::as_str),
                    event
                        .get("source_broadcaster_user_name")
                        .and_then(Value::as_str),
                )
                .filter(|source| {
                    broadcaster_login
                        .as_deref()
                        .is_none_or(|login| !source.eq_ignore_ascii_case(login))
                })
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
        timestamp: event.get("banned_at").and_then(Value::as_str)
            .or_else(|| root.get("metadata").and_then(|metadata| metadata.get("message_timestamp")).and_then(Value::as_str))
            .and_then(|timestamp| chrono::DateTime::parse_from_rfc3339(timestamp).ok())
            .map(|timestamp| timestamp.with_timezone(&chrono::Utc)),
        moderation_target_user_id: moderation_action_payload(event)
            .and_then(|payload| find_nested_string_field(payload, "user_id"))
            .or_else(|| event.get("user_id").and_then(Value::as_str).filter(|id| !id.is_empty()).map(str::to_owned)),
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

fn parse_moderation_action(event: &Value, timestamp: Option<&str>) -> String {
    let Some(action) = event.get("action") else {
        return "updated moderation state".to_owned();
    };

    if let Some(action_str) = action.as_str() {
        let trimmed = action_str.trim();
        if !trimmed.is_empty() {
            return format_moderation_action(trimmed, event.get(trimmed), timestamp);
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

fn format_moderation_action(
    action: &str,
    payload: Option<&Value>,
    timestamp: Option<&str>,
) -> String {
    let Some(payload) = payload else {
        return action.to_owned();
    };
    let value = match action.strip_prefix("shared_chat_").unwrap_or(action) {
        "timeout" => payload
            .get("duration_seconds")
            .or_else(|| payload.get("duration"))
            .and_then(Value::as_u64)
            .or_else(|| {
                let expires = payload
                    .get("expires_at")
                    .and_then(Value::as_str)
                    .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())?;
                let started =
                    timestamp.and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())?;
                let millis = (expires - started).num_milliseconds();
                u64::try_from(millis)
                    .ok()
                    .map(|millis| (millis + 500) / 1000)
            })
            .map(|seconds| format!("{action}_{seconds}s")),
        "slow" => payload
            .get("wait_time_seconds")
            .and_then(Value::as_u64)
            .map(|seconds| format!("{action}_{seconds}s")),
        "followers" => payload
            .get("follow_duration_minutes")
            .and_then(Value::as_u64)
            .map(|minutes| format!("{action}_{minutes}m")),
        _ => None,
    };
    value.unwrap_or_else(|| action.to_owned())
}

fn moderation_action_payload(event: &Value) -> Option<&Value> {
    let action = event.get("action")?;
    if let Some(name) = action.as_str() {
        event.get(name.trim())
    } else {
        Some(action)
    }
}

fn parse_moderation_target_message_id(event: &Value) -> Option<String> {
    if let Some(message_id) = moderation_action_payload(event)
        .and_then(|payload| find_nested_string_field(payload, "message_id"))
    {
        return Some(message_id);
    }
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
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio::time::Instant;
    use tokio_tungstenite::tungstenite::Message;

    use super::{
        parse_notice, subscription_specs, EventSubAuth, EventSubCommand, EventSubConnectOutcome,
        EventSubEvent, EventSubNoticeKind, EventSubSession, RecentEventIds,
    };

    fn test_session() -> (
        EventSubSession,
        mpsc::Sender<EventSubCommand>,
        mpsc::Receiver<EventSubEvent>,
    ) {
        let (event_tx, event_rx) = mpsc::channel(16);
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        (EventSubSession::new(event_tx, cmd_rx), cmd_tx, event_rx)
    }

    async fn wait_for_connection(events: &mut mpsc::Receiver<EventSubEvent>) -> bool {
        tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(event) = events.recv().await {
                if let EventSubEvent::Connected { resumed } = event {
                    return resumed;
                }
            }
            panic!("session stopped before connecting");
        })
        .await
        .expect("welcome was not processed")
    }

    async fn wait_for_pong(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        payload: &[u8],
    ) {
        tokio::time::timeout(Duration::from_secs(3), async {
            socket
                .send(Message::Ping(payload.to_vec().into()))
                .await
                .unwrap();
            loop {
                match socket.next().await {
                    Some(Ok(Message::Pong(received))) if received.as_slice() == payload => return,
                    Some(Ok(_)) => {}
                    other => panic!("socket closed before pong: {other:?}"),
                }
            }
        })
        .await
        .expect("ping was not processed");
    }

    #[tokio::test]
    async fn account_changes_cancel_a_stalled_initial_handshake() {
        for clear_auth in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            let (mut session, commands, _events) = test_session();
            session.apply_command(EventSubCommand::SetAuth {
                token: "old-token".into(),
                client_id: "client".into(),
                user_id: "old-user".into(),
                scopes: Vec::new(),
            });
            let client = tokio::spawn(async move {
                let result = session.connect_once(&url).await;
                (session, result)
            });
            let (_socket, _) = listener.accept().await.unwrap();
            let command = if clear_auth {
                EventSubCommand::ClearAuth
            } else {
                EventSubCommand::SetAuth {
                    token: "new-token".into(),
                    client_id: "client".into(),
                    user_id: "new-user".into(),
                    scopes: Vec::new(),
                }
            };
            commands.send(command).await.unwrap();
            let (session, result) = tokio::time::timeout(Duration::from_secs(3), client)
                .await.expect("account change waited for the connection timeout").unwrap();
            assert!(matches!(result, Ok(EventSubConnectOutcome::Reconnect {
                reconnect_url: None, immediate: true,
            })));
            assert_eq!(session.auth.as_ref().map(|auth| auth.user_id.as_str()),
                if clear_auth { None } else { Some("new-user") });
        }
    }

    #[tokio::test]
    async fn closing_commands_cancels_a_stalled_initial_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (mut session, commands, _events) = test_session();
        let client = tokio::spawn(async move { session.connect_once(&url).await });
        let (_socket, _) = listener.accept().await.unwrap();
        drop(commands);
        let result = tokio::time::timeout(Duration::from_secs(3), client)
            .await.expect("shutdown waited for the connection timeout").unwrap();
        assert!(matches!(result, Ok(EventSubConnectOutcome::Stop)));
    }

    #[tokio::test]
    async fn channel_changes_during_handshake_keep_the_original_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (mut session, commands, _events) = test_session();
        let client = tokio::spawn(async move {
            let result = session.connect_once(&url).await;
            (session, result)
        });
        let (_socket, _) = listener.accept().await.unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(5)).await;
        commands.send(EventSubCommand::WatchChannel {
            broadcaster_id: "channel".into(), can_moderate: false,
        }).await.unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::time::resume();
        let (session, result) = tokio::time::timeout(Duration::from_secs(3), client)
            .await.expect("channel command restarted the connection deadline").unwrap();
        assert!(matches!(result, Err(error) if error == "EventSub connection timed out"));
        assert!(session.watched_broadcasters.contains_key("channel"));
    }

    #[tokio::test]
    async fn missing_welcome_closes_idle_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (mut session, _commands, _events) = test_session();
        let client = tokio::spawn(async move { session.connect_once(&url).await });
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        wait_for_pong(&mut socket, b"handshake-complete").await;

        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(11)).await;
        tokio::time::resume();
        let outcome = tokio::time::timeout(Duration::from_secs(3), client)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(outcome, Err(error) if error == "EventSub welcome timed out"));
        assert!(matches!(
            socket.next().await,
            None | Some(Err(_)) | Some(Ok(Message::Close(_)))
        ));
    }

    #[tokio::test]
    async fn keepalive_deadline_tolerates_jitter_but_ignores_ping_and_invalid_json() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (mut session, _commands, mut events) = test_session();
        let client = tokio::spawn(async move { session.connect_once(&url).await });
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                json!({
                    "metadata": {"message_type": "session_welcome"},
                    "payload": {"session": {"id": "idle-test", "keepalive_timeout_seconds": 10}}
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        assert!(!wait_for_connection(&mut events).await);

        // Keep network I/O on the real clock; advance only the inactivity timer.
        for message_type in ["session_keepalive", "notification"] {
            tokio::time::pause();
            tokio::time::advance(Duration::from_millis(10_500)).await;
            assert!(
                !client.is_finished(),
                "the watchdog did not allow keepalive delivery jitter"
            );
            tokio::time::resume();
            wait_for_pong(&mut socket, b"waiting-for-delayed-keepalive").await;
            let payload = if message_type == "notification" {
                json!({"event": {
                    "broadcaster_user_id": "123",
                    "broadcaster_user_login": "example"
                }})
            } else {
                json!({})
            };
            socket
                .send(Message::Text(
                    json!({
                        "metadata": {
                            "message_type": message_type,
                            "subscription_type": "stream.online"
                        },
                        "payload": payload
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            wait_for_pong(&mut socket, message_type.as_bytes()).await;
        }

        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(!client.is_finished());
        tokio::time::resume();
        socket.send(Message::Text("{invalid".into())).await.unwrap();
        wait_for_pong(&mut socket, b"ping-is-not-keepalive").await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(4)).await;
        tokio::time::resume();

        let outcome = tokio::time::timeout(Duration::from_secs(3), client)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(outcome, Err(error) if error == "EventSub keepalive timed out"));
        assert!(matches!(
            socket.next().await,
            None | Some(Err(_)) | Some(Ok(Message::Close(_)))
        ));
    }

    #[tokio::test]
    async fn resumed_socket_uses_replacement_keepalive_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (mut session, _commands, mut events) = test_session();
        let client = tokio::spawn(async move { session.connect_once(&url).await });
        let (stream, _) = listener.accept().await.unwrap();
        let mut old_socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        old_socket
            .send(Message::Text(
                json!({
                    "metadata": {"message_type": "session_welcome"},
                    "payload": {"session": {"id": "old-socket", "keepalive_timeout_seconds": 10}}
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        assert!(!wait_for_connection(&mut events).await);

        let replacement = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let reconnect_url = format!("ws://{}", replacement.local_addr().unwrap());
        old_socket
            .send(Message::Text(
                json!({
                    "metadata": {"message_type": "session_reconnect"},
                    "payload": {"session": {"reconnect_url": reconnect_url}}
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let (stream, _) = replacement.accept().await.unwrap();
        let mut new_socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        new_socket
            .send(Message::Text(
                json!({
                    "metadata": {"message_type": "session_welcome"},
                    "payload": {"session": {"id": "new-socket", "keepalive_timeout_seconds": 30}}
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        assert!(wait_for_connection(&mut events).await);

        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(13)).await;
        tokio::time::resume();
        wait_for_pong(&mut new_socket, b"replacement-is-active").await;
        assert!(
            !client.is_finished(),
            "the old timeout survived the socket transfer"
        );
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(20)).await;
        tokio::time::resume();
        let outcome = tokio::time::timeout(Duration::from_secs(3), client)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(outcome, Err(error) if error == "EventSub keepalive timed out"));
    }

    #[tokio::test]
    async fn healthy_traffic_resets_backoff_but_short_or_idle_sessions_do_not() {
        for (elapsed_secs, message_type, expected_attempt) in [
            (1, Some("session_keepalive"), 4),
            (31, Some("session_keepalive"), 0),
            (31, Some("notification"), 0),
            (31, None, 4),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            let (mut session, _commands, mut events) = test_session();
            session.reconnect_attempt = 4;
            let client = tokio::spawn(async move {
                let outcome = session.connect_once(&url).await;
                (session, outcome)
            });
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket
                .send(Message::Text(
                    json!({
                        "metadata": {"message_type": "session_welcome"},
                        "payload": {"session": {"id": "backoff-test", "keepalive_timeout_seconds": 60}}
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            assert!(!wait_for_connection(&mut events).await);

            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(elapsed_secs)).await;
            tokio::time::resume();
            let frame = message_type.map_or_else(
                || "{invalid".to_owned(),
                |message_type| {
                    json!({
                        "metadata": {"message_type": message_type},
                        "payload": {}
                    })
                    .to_string()
                },
            );
            socket.send(Message::Text(frame.into())).await.unwrap();
            wait_for_pong(&mut socket, b"backoff-check").await;
            socket.close(None).await.unwrap();
            let (session, outcome) = tokio::time::timeout(Duration::from_secs(3), client)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                outcome,
                Ok(EventSubConnectOutcome::Reconnect {
                    immediate: false,
                    ..
                })
            ));
            assert_eq!(
                session.reconnect_attempt, expected_attempt,
                "unexpected retry delay after {elapsed_secs}s with {message_type:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_backoff_applies_channel_commands_and_wakes_for_auth() {
        let (mut session, commands, _events) = test_session();
        let sender = commands.clone();
        let producer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            for command in [
                EventSubCommand::WatchChannel {
                    broadcaster_id: " 123 ".into(),
                    can_moderate: false,
                },
                EventSubCommand::WatchChannel {
                    broadcaster_id: "456".into(),
                    can_moderate: true,
                },
                EventSubCommand::UnwatchChannel {
                    broadcaster_id: " 123 ".into(),
                },
                EventSubCommand::SetAuth {
                    token: "  oauth: new-token  ".into(),
                    client_id: " client ".into(),
                    user_id: " user ".into(),
                    scopes: vec![" USER:READ:WHISPERS ".into()],
                },
            ] {
                sender.send(command).await.unwrap();
            }
        });
        let start = Instant::now();
        let outcome = session.wait_for_reconnect(Duration::from_secs(30)).await;
        assert_eq!(start.elapsed(), Duration::from_secs(5));
        assert!(matches!(
            outcome,
            EventSubConnectOutcome::Reconnect {
                immediate: true,
                ..
            }
        ));
        assert!(!session.watched_broadcasters.contains_key("123"));
        assert!(session.watched_broadcasters["456"].can_moderate);
        let auth = session.auth.unwrap();
        assert_eq!(auth.token, "new-token");
        assert_eq!(auth.client_id, "client");
        assert_eq!(auth.user_id, "user");
        assert!(auth.has_scope("user:read:whispers"));
        producer.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_backoff_channel_updates_keep_original_deadline() {
        let (mut session, commands, _events) = test_session();
        let sender = commands.clone();
        let producer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(20)).await;
            sender
                .send(EventSubCommand::WatchChannel {
                    broadcaster_id: "123".into(),
                    can_moderate: false,
                })
                .await
                .unwrap();
        });
        let start = Instant::now();
        let outcome = session.wait_for_reconnect(Duration::from_secs(30)).await;
        assert_eq!(start.elapsed(), Duration::from_secs(30));
        assert!(matches!(
            outcome,
            EventSubConnectOutcome::Reconnect {
                immediate: false,
                ..
            }
        ));
        assert!(session.watched_broadcasters.contains_key("123"));
        producer.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_backoff_stops_immediately_when_command_input_closes() {
        let (mut session, commands, _events) = test_session();
        commands
            .send(EventSubCommand::WatchChannel {
                broadcaster_id: "123".into(),
                can_moderate: false,
            })
            .await
            .unwrap();
        drop(commands);
        let start = Instant::now();
        let outcome = session.wait_for_reconnect(Duration::from_secs(30)).await;
        assert_eq!(start.elapsed(), Duration::ZERO);
        assert!(matches!(outcome, EventSubConnectOutcome::Stop));
        assert!(session.watched_broadcasters.contains_key("123"));
    }

    async fn change_auth_on_open_socket(command: EventSubCommand) -> EventSubSession {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(8);
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
        let mut session = EventSubSession::new(event_tx, cmd_rx);
        session.auth = Some(auth("old-user", &["user:read:whispers"]));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            cmd_tx.send(command).await.unwrap();
            let closed = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
                .await
                .expect("old account socket stayed open");
            assert!(
                closed.is_none()
                    || matches!(
                        closed,
                        Some(Err(_)) | Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))
                    )
            );
        });
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.connect_once(&url),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(
            outcome,
            EventSubConnectOutcome::Reconnect {
                reconnect_url: None,
                immediate: true
            }
        ));
        server.await.unwrap();
        session
    }

    #[tokio::test]
    async fn account_switch_closes_previous_event_subscriptions() {
        let session = change_auth_on_open_socket(EventSubCommand::SetAuth {
            token: "  oauth: new-token  ".into(),
            client_id: "client".into(),
            user_id: "new-user".into(),
            scopes: vec!["user:read:whispers".into()],
        })
        .await;
        let auth = session.auth.unwrap();
        assert_eq!(auth.user_id, "new-user");
        assert_eq!(auth.token, "new-token");
    }

    #[tokio::test]
    async fn logout_closes_previous_event_subscriptions() {
        let session = change_auth_on_open_socket(EventSubCommand::ClearAuth).await;
        assert!(session.auth.is_none());
    }

    #[tokio::test]
    async fn invalid_auth_clears_previous_event_subscriptions() {
        let session = change_auth_on_open_socket(EventSubCommand::SetAuth {
            token: String::new(),
            client_id: "client".into(),
            user_id: "new-user".into(),
            scopes: vec![],
        })
        .await;
        assert!(session.auth.is_none());
    }

    #[test]
    fn event_deduplication_retains_recent_unique_ids() {
        let mut cache = RecentEventIds::new(2);
        cache.insert("first".into());
        cache.insert("second".into());
        cache.insert("first".into());
        assert!(cache.contains("first"));
        assert!(cache.contains("second"));
        cache.insert("third".into());
        assert!(!cache.contains("first"));
        assert!(cache.contains("second"));
        assert!(cache.contains("third"));
        cache.insert("first".into());
        assert!(!cache.contains("second"));
        assert!(cache.contains("first"));
        assert!(cache.contains("third"));
    }

    #[test]
    fn zero_capacity_event_cache_stays_empty() {
        let mut cache = RecentEventIds::new(0);
        cache.insert("event".into());
        assert!(!cache.contains("event"));
    }

    #[test]
    #[ignore = "manual comparison of full event ID caches"]
    fn perf_event_id_deduplication() {
        const CAPACITY: usize = 10_000;
        const EVENTS: usize = 20_000;
        let ids: Vec<String> = (0..CAPACITY + EVENTS)
            .map(|id| format!("{id:032x}"))
            .collect();
        let mut previous = ids[..CAPACITY].to_vec();
        let started = std::time::Instant::now();
        for id in &ids[CAPACITY..] {
            assert!(!std::hint::black_box(
                previous.iter().any(|entry| entry == id)
            ));
            previous.remove(0);
            previous.push(id.clone());
        }
        let previous_elapsed = started.elapsed();

        let mut cache = RecentEventIds::new(CAPACITY);
        for id in &ids[..CAPACITY] {
            cache.insert(id.clone());
        }
        let started = std::time::Instant::now();
        for id in &ids[CAPACITY..] {
            assert!(!std::hint::black_box(cache.contains(id)));
            cache.insert(id.clone());
        }
        let elapsed = started.elapsed();
        println!("[perf] event ID cache, {EVENTS} events at capacity {CAPACITY}: previous={previous_elapsed:?}, current={elapsed:?}");
    }

    fn auth(user_id: &str, scopes: &[&str]) -> EventSubAuth {
        EventSubAuth {
            token: "token".to_owned(),
            client_id: "client".to_owned(),
            user_id: user_id.to_owned(),
            scopes: scopes
                .iter()
                .map(|scope| scope.to_ascii_lowercase())
                .collect(),
        }
    }

    #[test]
    fn moderator_scoped_subscriptions_include_ban_and_moderate_topics() {
        let scopes = [
            "user:read:chat",
            "channel:moderate",
            "moderator:manage:automod",
            "moderator:read:suspicious_users",
            "moderator:read:blocked_terms",
            "moderator:read:chat_settings",
            "moderator:read:unban_requests",
            "moderator:read:banned_users",
            "moderator:read:chat_messages",
            "moderator:read:warnings",
            "moderator:read:moderators",
            "moderator:read:vips",
        ];
        let specs = subscription_specs("123", &auth("456", &scopes), true);
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
    fn non_moderator_channel_skips_moderation_topics() {
        let specs = subscription_specs(
            "123",
            &auth(
                "456",
                &[
                    "channel:moderate",
                    "moderator:manage:automod",
                    "moderator:read:suspicious_users",
                ],
            ),
            false,
        );
        let kinds: Vec<&str> = specs.iter().map(|s| s.kind).collect();

        assert!(!kinds.contains(&"channel.ban"));
        assert!(!kinds.contains(&"automod.message.hold"));
        assert!(!kinds.contains(&"channel.suspicious_user.message"));
    }

    #[test]
    fn broadcaster_only_topics_are_limited_to_self_channel() {
        let scopes = [
            "channel:read:subscriptions",
            "channel:read:redemptions",
            "channel:read:polls",
            "channel:read:predictions",
        ];
        let self_specs = subscription_specs("123", &auth("123", &scopes), false);
        let other_specs = subscription_specs("999", &auth("123", &scopes), false);
        let self_kinds: Vec<&str> = self_specs.iter().map(|s| s.kind).collect();
        let other_kinds: Vec<&str> = other_specs.iter().map(|s| s.kind).collect();

        assert!(self_kinds.contains(&"channel.subscribe"));
        assert!(self_kinds.contains(&"channel.channel_points_custom_reward_redemption.add"));
        assert!(self_kinds.contains(&"channel.poll.begin"));
        assert!(self_kinds.contains(&"channel.prediction.begin"));
        assert!(!other_kinds.contains(&"channel.subscribe"));
        assert!(!other_kinds.contains(&"channel.channel_points_custom_reward_redemption.add"));
        assert!(!other_kinds.contains(&"channel.poll.begin"));
        assert!(!other_kinds.contains(&"channel.prediction.begin"));
    }

    #[test]
    fn hype_train_topics_require_broadcaster_scope_and_use_current_version() {
        let broadcaster = auth("123", &["channel:read:hype_train"]);
        let specs = subscription_specs("123", &broadcaster, false);
        for kind in [
            "channel.hype_train.begin",
            "channel.hype_train.progress",
            "channel.hype_train.end",
        ] {
            let spec = specs
                .iter()
                .find(|spec| spec.kind == kind)
                .expect("hype subscription");
            assert_eq!(spec.version, "2");
            assert_eq!(spec.condition, json!({"broadcaster_user_id": "123"}));
        }
        for specs in [
            subscription_specs("123", &auth("123", &[]), false),
            subscription_specs("999", &broadcaster, true),
        ] {
            assert!(!specs
                .iter()
                .any(|spec| spec.kind.starts_with("channel.hype_train.")));
        }
    }

    #[test]
    fn foreign_channel_uses_polling_and_irc_for_presence_and_raid() {
        let specs = subscription_specs("999", &auth("123", &["user:read:chat"]), false);
        let kinds: Vec<&str> = specs.iter().map(|s| s.kind).collect();
        let estimated_cost: u32 = specs.iter().map(|s| s.estimated_cost).sum();

        assert!(!kinds.contains(&"stream.online"));
        assert!(!kinds.contains(&"stream.offline"));
        assert!(!kinds.contains(&"channel.raid"));
        assert!(kinds.contains(&"channel.chat.user_message_hold"));
        assert!(kinds.contains(&"channel.chat.user_message_update"));
        assert_eq!(estimated_cost, 0);
    }

    #[test]
    fn self_channel_keeps_zero_cost_presence_topics() {
        let specs = subscription_specs("123", &auth("123", &[]), false);
        let kinds: Vec<&str> = specs.iter().map(|s| s.kind).collect();

        assert!(kinds.contains(&"stream.online"));
        assert!(kinds.contains(&"stream.offline"));
        assert!(!kinds.contains(&"channel.raid"));
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
    fn moderation_preserves_source_timestamps_and_target_ids() {
        let ban = json!({
            "metadata": {"message_timestamp": "2026-01-01T00:01:00Z"},
            "payload": {"event": {"broadcaster_user_id": "123", "broadcaster_user_login": "room",
                "user_id": "10", "user_login": "alice", "banned_at": "2026-01-01T00:00:00Z"}}
        });
        let notice = parse_notice(&ban, "channel.ban").unwrap();
        assert_eq!(notice.timestamp.unwrap().to_rfc3339(), "2026-01-01T00:00:00+00:00");
        assert_eq!(notice.moderation_target_user_id.as_deref(), Some("10"));
        let action = json!({
            "metadata": {"message_timestamp": "2026-01-01T00:01:00Z"},
            "payload": {"event": {"broadcaster_user_id": "123", "broadcaster_user_login": "room",
                "moderator_user_id": "99", "action": "timeout", "timeout": {"user_id": "10", "user_login": "alice"}}}
        });
        let notice = parse_notice(&action, "channel.moderate").unwrap();
        assert_eq!(notice.timestamp.unwrap().to_rfc3339(), "2026-01-01T00:01:00+00:00");
        assert_eq!(notice.moderation_target_user_id.as_deref(), Some("10"));
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
                reason,
            } => {
                assert_eq!(moderator_login, "mod_jane");
                assert_eq!(action, "warn");
                assert_eq!(target_login.as_deref(), Some("viewer123"));
                assert_eq!(target_message_id, None);
                assert_eq!(source_channel_login, None);
                assert_eq!(reason, None);
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn moderation_string_actions_read_their_nested_targets() {
        for action in ["ban", "timeout", "delete", "warn"] {
            let mut event = json!({
                "broadcaster_user_id": "123",
                "broadcaster_user_login": "streamer",
                "source_broadcaster_user_login": "streamer",
                "moderator_user_login": "mod_jane",
                "action": action,
            });
            event[action] = json!({
                "user_id": "456",
                "user_login": "viewer123",
                "message_id": if action == "delete" { Some("deleted-message") } else { None },
                "expires_at": "2026-11-13T20:10:00Z",
                "reason": "  Repeated spam  ",
            });
            let payload = json!({
                "metadata": { "message_timestamp": "2026-11-13T20:00:00Z" },
                "payload": { "event": event },
            });
            let notice = parse_notice(&payload, "channel.moderate").unwrap();
            let EventSubNoticeKind::ModerationAction {
                action: parsed_action,
                target_login,
                target_message_id,
                source_channel_login,
                reason,
                ..
            } = notice.kind
            else {
                panic!("expected moderation action")
            };
            assert_eq!(target_login.as_deref(), Some("viewer123"));
            assert_eq!(reason.as_deref(), Some("Repeated spam"));
            assert_eq!(
                parsed_action,
                if action == "timeout" {
                    "timeout_600s"
                } else {
                    action
                }
            );
            assert_eq!(
                target_message_id.as_deref(),
                if action == "delete" {
                    Some("deleted-message")
                } else {
                    None
                }
            );
            assert!(source_channel_login.is_none());
        }
    }

    #[test]
    fn moderation_string_actions_keep_room_mode_parameters() {
        for (action, field, value, expected) in [
            ("slow", "wait_time_seconds", 30, "slow_30s"),
            ("followers", "follow_duration_minutes", 10, "followers_10m"),
        ] {
            let mut event = json!({ "broadcaster_user_id": "123", "action": action });
            event[action] = json!({});
            event[action][field] = json!(value);
            let notice =
                parse_notice(&json!({"payload": {"event": event}}), "channel.moderate").unwrap();
            assert!(
                matches!(notice.kind, EventSubNoticeKind::ModerationAction { action, .. } if action == expected)
            );
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
    fn parse_hype_train_lifecycle_preserves_progress_and_contributors() {
        for phase in ["begin", "progress", "end"] {
            let mut event = json!({
                "id": "train-1",
                "broadcaster_user_id": "123",
                "broadcaster_user_login": "streamer",
                "level": 3,
                "total": 4500,
                "top_contributions": [
                    {"user_login": "alice", "type": "bits", "total": 500},
                    {"user_login": "bob", "type": "subscription", "total": 1500}
                ],
                "started_at": "2026-01-01T00:00:00Z"
            });
            if phase == "end" {
                event["ended_at"] = json!("2026-01-01T00:05:00Z");
            } else {
                event["progress"] = json!(1000);
                event["goal"] = json!(2500);
                event["expires_at"] = json!("2026-01-01T00:05:00Z");
            }
            let payload =
                json!({"metadata": {"message_id": "event-1"}, "payload": {"event": event}});
            let notice = parse_notice(&payload, &format!("channel.hype_train.{phase}")).unwrap();
            assert_eq!(notice.event_id.as_deref(), Some("event-1"));
            match notice.kind {
                EventSubNoticeKind::HypeTrainLifecycle {
                    train_id,
                    phase: parsed_phase,
                    level,
                    progress,
                    goal,
                    total,
                    last_contribution_login,
                    top_contribution_login,
                    top_contribution_type,
                    top_contribution_total,
                    started_at,
                    ends_at,
                } => {
                    assert_eq!(train_id, "train-1");
                    assert_eq!(parsed_phase, phase);
                    assert_eq!(level, 3);
                    assert_eq!(total, 4500);
                    assert_eq!(
                        (progress, goal),
                        if phase == "end" { (0, 0) } else { (1000, 2500) }
                    );
                    assert_eq!(last_contribution_login, None);
                    assert_eq!(top_contribution_login.as_deref(), Some("bob"));
                    assert_eq!(top_contribution_type.as_deref(), Some("subscription"));
                    assert_eq!(top_contribution_total, Some(1500));
                    assert_eq!(started_at.as_deref(), Some("2026-01-01T00:00:00Z"));
                    assert_eq!(ends_at.as_deref(), Some("2026-01-01T00:05:00Z"));
                }
                other => panic!("unexpected kind: {other:?}"),
            }
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

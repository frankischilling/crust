mod bttv;
pub(crate) mod reconcile;
mod seventv;
mod seventv_pool;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crust_core::model::ChannelId;
use crust_emotes::providers::read_bounded_json;
use crust_emotes::EmoteInfo;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::seventv::{SevenTvCosmeticUpdate, SevenTvLiveCosmeticUpdate};

use reconcile::SnapshotTicket;

const MAX_TARGETS: usize = 128;
const MAX_SNAPSHOTS: usize = 4;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_secs(30);
const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Provider {
    Bttv,
    SevenTv,
}

impl Provider {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Bttv => "bttv",
            Self::SevenTv => "7tv",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EmoteDelta {
    Upsert {
        emote: EmoteInfo,
        old_code: Option<String>,
        /// A partial update omitted the active-set overlay field.
        preserve_overlay: bool,
    },
    Rename {
        id: String,
        old_code: Option<String>,
        code: String,
    },
    Overlay {
        id: String,
        code: Option<String>,
        zero_width: bool,
    },
    Remove {
        id: String,
        code: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LiveSource {
    pub(crate) provider: Provider,
    pub(crate) room_id: String,
    pub(crate) platform: String,
    pub(crate) user_id: Option<String>,
    pub(crate) set_id: Option<String>,
    pub(crate) connection_index: Option<usize>,
}

impl LiveSource {
    pub(crate) fn same_catalog(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.room_id == other.room_id
            && self.platform == other.platform
            && self.set_id == other.set_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LiveTarget {
    pub(crate) scope: LiveScope,
    pub(crate) generation: u64,
    pub(crate) binding_generation: u64,
    pub(crate) source: LiveSource,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum LiveScope {
    Channel(ChannelId),
    Global,
    Personal(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LiveBinding {
    pub(crate) generation: u64,
    pub(crate) source: LiveSource,
}

enum Snapshot {
    Emotes(Vec<EmoteInfo>),
    Source(LiveSource),
}

/// Desired subscriptions replace one another; channel churn cannot accumulate commands.
pub(crate) struct LiveEmoteUpdates {
    bttv: watch::Sender<Vec<LiveTarget>>,
    seven_tv: watch::Sender<Vec<LiveTarget>>,
}

impl LiveEmoteUpdates {
    pub(crate) fn spawn(
        index: crate::EmoteIndex,
        cosmetics: mpsc::Sender<SevenTvCosmeticUpdate>,
    ) -> (Self, mpsc::Receiver<()>) {
        let (dirty, received) = mpsc::channel(1);
        let (bttv, bttv_rx) = watch::channel(Vec::new());
        let (seven_tv, seven_tv_rx) = watch::channel(Vec::new());
        tokio::spawn(run_worker(
            Provider::Bttv,
            Endpoints::production(Provider::Bttv),
            index.clone(),
            dirty.clone(),
            bttv_rx,
            None,
        ));
        tokio::spawn(run_worker(
            Provider::SevenTv,
            Endpoints::production(Provider::SevenTv),
            index,
            dirty,
            seven_tv_rx,
            Some(cosmetics),
        ));
        (Self { bttv, seven_tv }, received)
    }

    /// Called on the existing catalog tick. This only copies source metadata.
    pub(crate) fn sync(&self, index: &crate::EmoteIndex) {
        let targets = index.read().unwrap().live_targets();
        for (provider, sender) in [
            (Provider::Bttv, &self.bttv),
            (Provider::SevenTv, &self.seven_tv),
        ] {
            let next: Vec<_> = targets
                .iter()
                .filter(|target| target.source.provider == provider)
                .cloned()
                .collect();
            sender.send_if_modified(|current| {
                if *current == next {
                    false
                } else {
                    *current = next;
                    true
                }
            });
        }
    }
}

#[derive(Clone)]
struct Endpoints {
    socket: String,
    bttv_api: String,
    seven_tv_gql: String,
}

impl Endpoints {
    fn production(provider: Provider) -> Self {
        Self {
            socket: match provider {
                Provider::Bttv => bttv::URL,
                Provider::SevenTv => seventv::URL,
            }
            .into(),
            bttv_api: "https://api.betterttv.net/3/cached/users/twitch".into(),
            seven_tv_gql: "https://7tv.io/v3/gql".into(),
        }
    }
}

async fn run_worker(
    provider: Provider,
    endpoints: Endpoints,
    index: crate::EmoteIndex,
    dirty: mpsc::Sender<()>,
    mut desired: watch::Receiver<Vec<LiveTarget>>,
    cosmetics: Option<mpsc::Sender<SevenTvCosmeticUpdate>>,
) {
    if provider == Provider::SevenTv {
        seventv_pool::run(endpoints, index, dirty, desired, cosmetics).await;
        return;
    }
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(12))
        .build()
    {
        Ok(client) => client,
        Err(_) => return,
    };
    let mut attempts = 0u32;
    loop {
        if desired.borrow().is_empty() {
            if desired.changed().await.is_err() {
                return;
            }
            continue;
        }
        let config = WebSocketConfig {
            max_message_size: Some(1024 * 1024),
            max_frame_size: Some(1024 * 1024),
            ..WebSocketConfig::default()
        };
        let connecting = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async_with_config(&endpoints.socket, Some(config), false),
        );
        tokio::pin!(connecting);
        let connection = loop {
            tokio::select! {
                changed = desired.changed() => {
                    if changed.is_err() { return; }
                    if desired.borrow_and_update().is_empty() { break None; }
                }
                connection = &mut connecting => break Some(connection),
            }
        };
        if let Some(Ok(Ok((socket, _)))) = connection {
            let epoch = NEXT_EPOCH.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                provider = provider.name(),
                epoch,
                "Live emote socket connected"
            );
            let started = Instant::now();
            run_connected(
                socket,
                &mut desired,
                epoch,
                ConnectionContext {
                    provider,
                    endpoints: &endpoints,
                    client: &client,
                    index: &index,
                    dirty: &dirty,
                },
            )
            .await;
            tracing::debug!(
                provider = provider.name(),
                epoch,
                "Live emote socket closed"
            );
            if started.elapsed() >= Duration::from_secs(30) {
                attempts = 0;
            }
        } else {
            tracing::debug!(
                provider = provider.name(),
                "Live emote socket connection did not complete"
            );
        }
        if desired.has_changed().is_err() {
            return;
        }
        attempts = attempts.saturating_add(1);
        let backoff = Duration::from_secs(1u64 << attempts.saturating_sub(1).min(6));
        tokio::select! {
            changed = desired.changed() => { if changed.is_err() { return; } }
            _ = tokio::time::sleep(backoff) => {}
        }
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct ConnectionContext<'a> {
    provider: Provider,
    endpoints: &'a Endpoints,
    client: &'a reqwest::Client,
    index: &'a crate::EmoteIndex,
    dirty: &'a mpsc::Sender<()>,
}

struct Watched {
    target: LiveTarget,
    next_snapshot: Instant,
    in_flight: bool,
}

struct Loaded {
    target: LiveTarget,
    ticket: SnapshotTicket,
    result: Option<Snapshot>,
}

async fn write(socket: &mut Socket, message: Message) -> Result<(), ()> {
    tokio::time::timeout(WRITE_TIMEOUT, socket.send(message))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())
}

async fn send_cosmetic_update(
    sender: Option<&mpsc::Sender<SevenTvCosmeticUpdate>>,
    epoch: u64,
    update: SevenTvLiveCosmeticUpdate,
) -> bool {
    let Some(sender) = sender else {
        return true;
    };
    matches!(
        tokio::time::timeout(
            WRITE_TIMEOUT,
            sender.send(SevenTvCosmeticUpdate::Live { epoch, update })
        )
        .await,
        Ok(Ok(()))
    )
}

async fn run_connected(
    mut socket: Socket,
    desired: &mut watch::Receiver<Vec<LiveTarget>>,
    epoch: u64,
    context: ConnectionContext<'_>,
) {
    let ConnectionContext {
        provider,
        endpoints,
        client,
        index,
        dirty,
    } = context;
    let mut watched = HashMap::<LiveScope, Watched>::new();
    let mut rooms = HashSet::<String>::new();
    let mut jobs = JoinSet::<Loaded>::new();
    let mut last_received = Instant::now();
    let mut last_ping = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut refresh_subscriptions = true;
    loop {
        if refresh_subscriptions {
            refresh_subscriptions = false;
            let next: Vec<_> = desired
                .borrow_and_update()
                .iter()
                .filter(|target| target.source.provider == Provider::Bttv)
                .take(MAX_TARGETS)
                .cloned()
                .collect();
            if next.is_empty() {
                break;
            }
            if next.iter().any(|target| {
                watched.get(&target.scope).is_some_and(|old| {
                    old.target.generation != target.generation
                        || old.target.binding_generation != target.binding_generation
                })
            }) {
                break;
            }
            let next_rooms: HashSet<_> = next
                .iter()
                .map(|target| target.source.room_id.clone())
                .collect();
            for room in rooms.difference(&next_rooms) {
                if write(&mut socket, Message::Text(bttv::unsubscribe(room).into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            for room in next_rooms.difference(&rooms) {
                if write(&mut socket, Message::Text(bttv::subscribe(room).into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            rooms = next_rooms;
            watched.retain(|_, old| next.iter().any(|target| target == &old.target));
            for target in next {
                watched.entry(target.scope.clone()).or_insert_with(|| {
                    index.write().unwrap().begin_live_epoch(&target, epoch);
                    Watched {
                        target,
                        next_snapshot: Instant::now(),
                        in_flight: false,
                    }
                });
            }
            tracing::debug!(
                provider = provider.name(),
                scopes = watched.len(),
                subscriptions = rooms.len(),
                "Live emote subscriptions sent"
            );
        }

        for entry in watched.values_mut() {
            if jobs.len() >= MAX_SNAPSHOTS {
                break;
            }
            if entry.in_flight || Instant::now() < entry.next_snapshot {
                continue;
            }
            let Some(ticket) = index
                .read()
                .unwrap()
                .live_snapshot_ticket(&entry.target, epoch)
            else {
                continue;
            };
            entry.in_flight = true;
            let target = entry.target.clone();
            let client = client.clone();
            let endpoint = endpoints.bttv_api.clone();
            jobs.spawn(async move {
                let result = fetch_snapshot(&client, &endpoint, &target).await;
                Loaded {
                    target,
                    ticket,
                    result,
                }
            });
        }

        tokio::select! {
            changed = desired.changed() => {
                if changed.is_err() { break; }
                refresh_subscriptions = true;
            }
            Some(completed) = jobs.join_next(), if !jobs.is_empty() => {
                let Ok(loaded) = completed else { break; };
                let Some(entry) = watched.get_mut(&loaded.target.scope).filter(|entry| entry.target == loaded.target) else { continue; };
                entry.in_flight = false;
                entry.next_snapshot = Instant::now() + if loaded.result.is_some() {
                    REFRESH_INTERVAL
                } else {
                    RETRY_INTERVAL
                };
                tracing::debug!(provider = provider.name(), room_id = %loaded.target.source.room_id,
                    succeeded = loaded.result.is_some(), "Live emote snapshot finished");
                if let Some(Snapshot::Emotes(emotes)) = loaded.result {
                    let result = index.write().unwrap().apply_live_snapshot(
                        &loaded.target, loaded.ticket, emotes, false);
                    if result.needs_snapshot { entry.next_snapshot = Instant::now() + RETRY_INTERVAL; }
                    if result.changed { let _ = dirty.try_send(()); }
                }
            }
            incoming = socket.next() => {
                let Some(Ok(message)) = incoming else { break; };
                last_received = Instant::now();
                match message {
                    Message::Ping(payload) => {
                        if write(&mut socket, Message::Pong(payload)).await.is_err() { break; }
                    }
                    Message::Close(_) => break,
                    Message::Text(text) => {
                        if let Some((room, deltas)) = bttv::decode(&text) {
                            let targets: Vec<_> = watched.values()
                                .filter(|entry| entry.target.source.room_id == room)
                                .map(|entry| entry.target.clone()).collect();
                            apply_deltas(index, dirty, &mut watched, &targets, epoch, &deltas);
                        }
                    }
                    _ => {}
                }
            }
            _ = tick.tick() => {
                if last_received.elapsed() >= Duration::from_secs(90) { break; }
                if last_ping.elapsed() >= Duration::from_secs(30) {
                    if write(&mut socket, Message::Ping(Vec::new().into())).await.is_err() { break; }
                    last_ping = Instant::now();
                }
            }
        }
    }
    // Dropping the join set cancels metadata jobs from this socket epoch.
}

fn apply_deltas(
    index: &crate::EmoteIndex,
    dirty: &mpsc::Sender<()>,
    watched: &mut HashMap<LiveScope, Watched>,
    targets: &[LiveTarget],
    epoch: u64,
    deltas: &[EmoteDelta],
) {
    let mut changed = false;
    for target in targets {
        for delta in deltas {
            let result = index
                .write()
                .unwrap()
                .apply_live_delta(target, epoch, delta.clone());
            changed |= result.changed;
            if result.needs_snapshot {
                if let Some(entry) = watched.get_mut(&target.scope) {
                    // A full journal pauses deltas until a bounded resync can reconcile it.
                    entry.next_snapshot = entry.next_snapshot.min(Instant::now() + RETRY_INTERVAL);
                }
            }
        }
    }
    if changed {
        let _ = dirty.try_send(());
    }
}

async fn fetch_snapshot(
    client: &reqwest::Client,
    endpoint: &str,
    target: &LiveTarget,
) -> Option<Snapshot> {
    let source = &target.source;
    if source.provider == Provider::SevenTv {
        return seventv::snapshot_at(client, endpoint, &target.scope, source).await;
    }
    let response = client
        .get(format!("{endpoint}/{}", source.room_id))
        .send()
        .await
        .ok()?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Some(Snapshot::Emotes(Vec::new()));
    }
    let response = response.error_for_status().ok()?;
    let data = read_bounded_json::<Value>(response, 8 * 1024 * 1024).await?;
    let mut emotes = Vec::new();
    for field in ["channelEmotes", "sharedEmotes"] {
        for emote in data.get(field)?.as_array()? {
            emotes.push(bttv::parse_emote(emote)?);
            if emotes.len() > 20_000 {
                return None;
            }
        }
    }
    Some(Snapshot::Emotes(emotes))
}

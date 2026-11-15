//! Share each live topic across a bounded set of sockets. Scopes that cannot
//! obtain live coverage still receive bounded metadata and cosmetic refreshes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};

use super::{
    fetch_snapshot, send_cosmetic_update, seventv, write, Endpoints, LiveScope, LiveTarget, Loaded,
    Snapshot, CONNECT_TIMEOUT, MAX_SNAPSHOTS, NEXT_EPOCH, REFRESH_INTERVAL, RETRY_INTERVAL,
    WRITE_TIMEOUT,
};
use crate::seventv::{
    load_7tv_cosmetics_catalog_at, next_cosmetic_catalog_generation, SevenTvBadgeMeta,
    SevenTvCosmeticUpdate, SevenTvLiveCosmeticUpdate, SevenTvPaintMeta,
};

const MAX_CONNECTIONS: usize = 8;
const MAX_SOCKET_EVENTS: usize = 128;
const MAX_RETIRED_TOPICS: usize = 512;

#[derive(Clone, Default)]
struct SocketPlan {
    revision: u64,
    epoch: Option<u64>,
    topics: HashSet<seventv::Topic>,
}

enum SocketEvent {
    Hello {
        slot: usize,
        revision: u64,
        epoch: u64,
        limit: usize,
    },
    Frame {
        slot: usize,
        revision: u64,
        epoch: u64,
        frame: seventv::Frame,
    },
    Closed {
        slot: usize,
        revision: u64,
        epoch: u64,
    },
    Barrier(oneshot::Sender<()>),
}

struct Slot {
    plan: SocketPlan,
    sender: watch::Sender<SocketPlan>,
    task: JoinHandle<()>,
    capacity: Option<usize>,
    acknowledged: HashSet<seventv::Topic>,
    retired: HashSet<seventv::Topic>,
    last_epoch: u64,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Watched {
    target: LiveTarget,
    topics: Vec<seventv::Topic>,
    epoch: u64,
    next_snapshot: Instant,
    in_flight: bool,
    refresh_after_load: bool,
    waiting_since: Instant,
    live: bool,
}

struct Pool {
    endpoints: Endpoints,
    client: reqwest::Client,
    index: crate::EmoteIndex,
    dirty: mpsc::Sender<()>,
    cosmetics: Option<mpsc::Sender<SevenTvCosmeticUpdate>>,
    events: mpsc::Sender<SocketEvent>,
    slots: BTreeMap<usize, Slot>,
    watched: HashMap<LiveScope, Watched>,
    order: Vec<LiveScope>,
    jobs: JoinSet<Loaded>,
    cosmetic_jobs: JoinSet<CatalogLoad>,
    next_cosmetic_refresh: Instant,
    force_cosmetic_refresh: bool,
    cosmetic_recovery: HashSet<LiveScope>,
    reported_uncovered: usize,
}

struct CatalogLoad {
    generation: u64,
    result: Option<(
        HashMap<String, SevenTvBadgeMeta>,
        HashMap<String, SevenTvPaintMeta>,
    )>,
}

fn next_epoch() -> u64 {
    NEXT_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub(super) async fn run(
    endpoints: Endpoints,
    index: crate::EmoteIndex,
    dirty: mpsc::Sender<()>,
    mut desired: watch::Receiver<Vec<LiveTarget>>,
    cosmetics: Option<mpsc::Sender<SevenTvCosmeticUpdate>>,
) {
    let Ok(client) = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(12))
        .build()
    else {
        return;
    };
    let mut recovering = false;
    loop {
        let (events, mut received) = mpsc::channel(MAX_SOCKET_EVENTS);
        let mut pool = Pool {
            endpoints: endpoints.clone(),
            client: client.clone(),
            index: index.clone(),
            dirty: dirty.clone(),
            cosmetics: cosmetics.clone(),
            events,
            slots: BTreeMap::new(),
            watched: HashMap::new(),
            order: Vec::new(),
            jobs: JoinSet::new(),
            cosmetic_jobs: JoinSet::new(),
            next_cosmetic_refresh: Instant::now(),
            force_cosmetic_refresh: false,
            cosmetic_recovery: HashSet::new(),
            reported_uncovered: 0,
        };
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let initial = desired.borrow_and_update().clone();
        if pool.sync_targets(initial).await.is_err() {
            return;
        }
        if recovering {
            pool.cosmetic_recovery.extend(
                pool.watched
                    .iter()
                    .filter(|(_, entry)| {
                        entry
                            .topics
                            .iter()
                            .any(|topic| matches!(topic.condition, seventv::Condition::Channel(_)))
                    })
                    .map(|(scope, _)| scope.clone()),
            );
        }
        let closed = loop {
            // A reader can lose even its Closed notification to backpressure.
            // Task completion is an independent liveness signal.
            if pool.slots.values().any(|slot| slot.task.is_finished()) {
                break false;
            }
            if pool.schedule_snapshots().await.is_err() {
                break false;
            }
            tokio::select! {
                changed = desired.changed() => {
                    if changed.is_err() { break true; }
                    let targets = desired.borrow_and_update().clone();
                    if pool.sync_targets(targets).await.is_err() { break false; }
                }
                Some(event) = received.recv() => {
                    if pool.on_socket_event(event).await.is_err() { break false; }
                }
                Some(loaded) = pool.jobs.join_next(), if !pool.jobs.is_empty() => {
                    if let Ok(loaded) = loaded { pool.complete_snapshot(loaded); }
                    else { break false; }
                }
                Some(loaded) = pool.cosmetic_jobs.join_next(), if !pool.cosmetic_jobs.is_empty() => {
                    match loaded {
                        Ok(loaded) => {
                            if pool.complete_catalog(loaded).await.is_err() { break false; }
                        }
                        Err(_) => break false,
                    }
                }
                _ = tick.tick() => {}
            }
        };
        // Stop readers before a replacement pool can publish another epoch.
        for slot in pool.slots.values() {
            slot.task.abort();
        }
        for slot in pool.slots.values_mut() {
            let _ = (&mut slot.task).await;
        }
        drop(pool);
        if closed {
            return;
        }
        recovering = true;
        // A blocked cosmetic consumer must not permanently stop live coverage.
        tokio::select! {
            changed = desired.changed() => { if changed.is_err() { return; } }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

impl Pool {
    async fn cosmetic(&self, epoch: u64, update: SevenTvLiveCosmeticUpdate) -> Result<(), ()> {
        send_cosmetic_update(self.cosmetics.as_ref(), epoch, update)
            .await
            .then_some(())
            .ok_or(())
    }

    fn remember_cosmetic_gap(&mut self, topics: &HashSet<seventv::Topic>) {
        self.cosmetic_recovery.extend(
            self.watched
                .iter()
                .filter(|(_, entry)| {
                    entry
                        .topics
                        .iter()
                        .any(|topic| matches!(topic.condition, seventv::Condition::Channel(_)))
                        && entry.topics.iter().any(|topic| topics.contains(topic))
                })
                .map(|(scope, _)| scope.clone()),
        );
    }

    fn renew_scopes(&mut self, topics: &HashSet<seventv::Topic>) {
        for entry in self.watched.values_mut() {
            if entry.topics.iter().any(|topic| topics.contains(topic)) {
                entry.epoch = next_epoch();
                entry.next_snapshot = Instant::now();
                entry.in_flight = false;
                entry.refresh_after_load = false;
                entry.waiting_since = Instant::now();
                entry.live = false;
                self.index
                    .write()
                    .unwrap()
                    .begin_live_epoch(&entry.target, entry.epoch);
            }
        }
    }

    async fn restart_slot(&mut self, id: usize) -> Result<(), ()> {
        let Some(slot) = self.slots.get_mut(&id) else {
            return Ok(());
        };
        let old_epoch = slot.plan.epoch.take();
        let affected = slot.plan.topics.clone();
        slot.plan.revision = next_epoch();
        slot.acknowledged.clear();
        slot.retired.clear();
        slot.sender.send_replace(slot.plan.clone());
        if old_epoch.is_some() {
            self.remember_cosmetic_gap(&affected);
        }
        self.renew_scopes(&affected);
        if let Some(epoch) = old_epoch {
            self.cosmetic(
                epoch,
                SevenTvLiveCosmeticUpdate::DisconnectedSlot { slot: id },
            )
            .await?;
        }
        Ok(())
    }

    async fn sync_targets(&mut self, targets: Vec<LiveTarget>) -> Result<(), ()> {
        let targets: Vec<_> = targets
            .into_iter()
            .filter(|target| target.source.provider == super::Provider::SevenTv)
            .collect();
        let replaced: HashSet<_> = targets
            .iter()
            .filter_map(|target| {
                self.watched
                    .get(&target.scope)
                    .filter(|old| old.target != *target)
            })
            .flat_map(|old| old.topics.iter().cloned())
            .collect();
        let restart: Vec<_> = self
            .slots
            .iter()
            .filter(|(_, slot)| !slot.plan.topics.is_disjoint(&replaced))
            .map(|(id, _)| *id)
            .collect();
        for id in restart {
            self.restart_slot(id).await?;
        }
        self.watched
            .retain(|_, old| targets.iter().any(|target| target == &old.target));
        self.order = targets.iter().map(|target| target.scope.clone()).collect();
        for target in targets {
            if self.watched.contains_key(&target.scope) {
                continue;
            }
            let epoch = next_epoch();
            self.index.write().unwrap().begin_live_epoch(&target, epoch);
            self.watched.insert(
                target.scope.clone(),
                Watched {
                    topics: seventv::topics(&target),
                    target,
                    epoch,
                    next_snapshot: Instant::now(),
                    in_flight: false,
                    refresh_after_load: false,
                    waiting_since: Instant::now(),
                    live: false,
                },
            );
        }
        self.cosmetic_recovery
            .retain(|scope| self.watched.contains_key(scope));
        self.assign_topics().await
    }

    fn start_slot(&mut self) {
        let Some(id) = (0..MAX_CONNECTIONS).find(|id| !self.slots.contains_key(id)) else {
            return;
        };
        let plan = SocketPlan {
            revision: next_epoch(),
            ..Default::default()
        };
        let (sender, received) = watch::channel(plan.clone());
        let task = tokio::spawn(run_socket(
            id,
            self.endpoints.socket.clone(),
            self.events.clone(),
            received,
        ));
        self.slots.insert(
            id,
            Slot {
                plan,
                sender,
                task,
                capacity: None,
                acknowledged: HashSet::new(),
                retired: HashSet::new(),
                last_epoch: 0,
            },
        );
    }

    async fn assign_topics(&mut self) -> Result<(), ()> {
        let mut needed = Vec::new();
        let mut seen = HashSet::new();
        for scope in &self.order {
            for topic in &self.watched[scope].topics {
                if seen.insert(topic.clone()) {
                    needed.push(topic.clone());
                }
            }
        }
        if !needed.is_empty() && self.slots.is_empty() {
            self.start_slot();
        }
        let mut assignments = HashMap::<usize, HashSet<seventv::Topic>>::new();
        let mut owners = HashSet::new();
        // Keep existing owners so an unrelated channel cannot move a live topic
        // to another socket or repeat its subscription.
        for (id, slot) in &self.slots {
            let assigned = assignments.entry(*id).or_default();
            for topic in &needed {
                if assigned.len() >= slot.capacity.unwrap_or(0) {
                    break;
                }
                if slot.plan.topics.contains(topic) && owners.insert(topic.clone()) {
                    assigned.insert(topic.clone());
                }
            }
        }
        for topic in &needed {
            if owners.contains(topic) {
                continue;
            }
            if let Some((id, _)) = self.slots.iter().find(|(id, slot)| {
                assignments.get(*id).map_or(0, HashSet::len) < slot.capacity.unwrap_or(0)
            }) {
                assignments.entry(*id).or_default().insert(topic.clone());
                owners.insert(topic.clone());
            }
        }
        let mut restart = Vec::new();
        for (id, slot) in &mut self.slots {
            let next = assignments.remove(id).unwrap_or_default();
            if next == slot.plan.topics {
                continue;
            }
            slot.retired
                .extend(slot.plan.topics.difference(&next).cloned());
            let needs_restart =
                !next.is_disjoint(&slot.retired) || slot.retired.len() > MAX_RETIRED_TOPICS;
            if needs_restart {
                restart.push(*id);
            }
            slot.acknowledged.retain(|topic| next.contains(topic));
            slot.plan.topics = next;
            if !needs_restart {
                slot.sender.send_replace(slot.plan.clone());
            }
        }
        for id in restart {
            self.restart_slot(id).await?;
        }
        let unassigned = needed.len().saturating_sub(owners.len());
        let unused: Vec<_> = self
            .slots
            .iter()
            .filter(|(_, slot)| {
                slot.plan.topics.is_empty() && (needed.is_empty() || unassigned == 0)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in unused {
            let mut slot = self.slots.remove(&id).unwrap();
            slot.task.abort();
            let _ = (&mut slot.task).await;
            if let Some(epoch) = slot.plan.epoch {
                self.cosmetic(
                    epoch,
                    SevenTvLiveCosmeticUpdate::DisconnectedSlot { slot: id },
                )
                .await?;
            }
        }
        if unassigned != 0
            && self.slots.len() < MAX_CONNECTIONS
            && self.slots.values().all(|slot| slot.capacity.is_some())
        {
            self.start_slot();
        }
        let uncovered = if self.slots.len() == MAX_CONNECTIONS
            && self.slots.values().all(|slot| slot.capacity.is_some())
        {
            unassigned
        } else {
            0
        };
        if uncovered != self.reported_uncovered {
            self.reported_uncovered = uncovered;
            tracing::warn!(
                uncovered_topics = uncovered,
                connections = self.slots.len(),
                "Live emote coverage changed; uncovered scopes use periodic refresh"
            );
        }
        Ok(())
    }

    fn is_current(&self, id: usize, revision: u64, epoch: u64) -> bool {
        self.slots
            .get(&id)
            .is_some_and(|slot| slot.plan.revision == revision && slot.plan.epoch == Some(epoch))
    }

    async fn on_socket_event(&mut self, event: SocketEvent) -> Result<(), ()> {
        match event {
            SocketEvent::Barrier(done) => {
                let _ = done.send(());
            }
            SocketEvent::Hello {
                slot: id,
                revision,
                epoch,
                limit,
            } => {
                let Some(slot) = self
                    .slots
                    .get_mut(&id)
                    .filter(|slot| slot.plan.revision == revision && epoch > slot.last_epoch)
                else {
                    return Ok(());
                };
                let affected = slot.plan.topics.clone();
                slot.plan.epoch = Some(epoch);
                slot.last_epoch = epoch;
                slot.capacity = Some(limit);
                slot.acknowledged.clear();
                slot.retired.clear();
                self.renew_scopes(&affected);
                self.cosmetic(epoch, SevenTvLiveCosmeticUpdate::ConnectedSlot { slot: id })
                    .await?;
                self.assign_topics().await?;
                if let Some(slot) = self.slots.get(&id) {
                    slot.sender.send_replace(slot.plan.clone());
                }
            }
            SocketEvent::Closed {
                slot: id,
                revision,
                epoch,
            } => {
                if !self.is_current(id, revision, epoch) {
                    return Ok(());
                }
                let slot = self.slots.get_mut(&id).unwrap();
                let affected = slot.plan.topics.clone();
                slot.plan.epoch = None;
                slot.acknowledged.clear();
                slot.sender.send_replace(slot.plan.clone());
                self.remember_cosmetic_gap(&affected);
                self.renew_scopes(&affected);
                self.cosmetic(
                    epoch,
                    SevenTvLiveCosmeticUpdate::DisconnectedSlot { slot: id },
                )
                .await?;
            }
            SocketEvent::Frame {
                slot: id,
                revision,
                epoch,
                frame,
            } => {
                if !self.is_current(id, revision, epoch) {
                    return Ok(());
                }
                match frame {
                    seventv::Frame::Subscribed(topic) => {
                        let slot = self.slots.get_mut(&id).unwrap();
                        if slot.plan.topics.contains(&topic) {
                            slot.acknowledged.insert(topic);
                        }
                        let ready: Vec<_> = self
                            .watched
                            .iter()
                            .filter(|(_, entry)| {
                                !entry.live
                                    && entry.topics.iter().all(|topic| self.topic_ready(topic))
                            })
                            .map(|(scope, _)| scope.clone())
                            .collect();
                        for scope in ready {
                            let entry = self.watched.get_mut(&scope).unwrap();
                            entry.live = true;
                            entry.next_snapshot = Instant::now();
                            entry.refresh_after_load |= entry.in_flight;
                        }
                    }
                    seventv::Frame::Updates(updates) => {
                        self.apply_updates(id, epoch, updates).await?
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn topic_ready(&self, topic: &seventv::Topic) -> bool {
        self.slots.values().any(|slot| {
            slot.plan.epoch.is_some()
                && slot.plan.topics.contains(topic)
                && slot.acknowledged.contains(topic)
        })
    }

    async fn schedule_snapshots(&mut self) -> Result<(), ()> {
        let now = Instant::now();
        if !self.cosmetic_recovery.is_empty()
            && self.cosmetic_recovery.iter().all(|scope| {
                self.watched[scope]
                    .topics
                    .iter()
                    .all(|topic| self.topic_ready(topic))
            })
        {
            // A fallback request may have started before the final ACK. Keep a
            // follow-up due even while that older request is still in flight.
            self.cosmetic_recovery.clear();
            self.force_cosmetic_refresh = true;
            self.next_cosmetic_refresh = now;
        }
        let polling = self.watched.values().any(|entry| {
            now.duration_since(entry.waiting_since) >= CONNECT_TIMEOUT
                && entry.topics.iter().any(|topic| !self.topic_ready(topic))
        });
        if (polling || self.force_cosmetic_refresh)
            && now >= self.next_cosmetic_refresh
            && self.cosmetic_jobs.is_empty()
            && self.jobs.len() < MAX_SNAPSHOTS
        {
            // If this fallback starts before subscriptions are ready, a later
            // ACK must still trigger a baseline that covers the intervening gap.
            let uncovered: HashSet<_> = self
                .watched
                .values()
                .flat_map(|entry| entry.topics.iter())
                .filter(|topic| !self.topic_ready(topic))
                .cloned()
                .collect();
            self.remember_cosmetic_gap(&uncovered);
            self.cosmetic(0, SevenTvLiveCosmeticUpdate::RefreshAll)
                .await?;
            let generation = next_cosmetic_catalog_generation();
            self.cosmetic(
                0,
                SevenTvLiveCosmeticUpdate::CatalogRefreshStarted { generation },
            )
            .await?;
            let client = self.client.clone();
            let endpoint = self.endpoints.seven_tv_gql.clone();
            self.cosmetic_jobs.spawn(async move {
                CatalogLoad {
                    generation,
                    result: load_7tv_cosmetics_catalog_at(&client, &endpoint).await,
                }
            });
            self.next_cosmetic_refresh = now + REFRESH_INTERVAL;
            self.force_cosmetic_refresh = false;
        }
        let due: Vec<_> = self
            .order
            .iter()
            .filter_map(|scope| {
                let entry = &self.watched[scope];
                if entry.in_flight || now < entry.next_snapshot {
                    return None;
                }
                let live = entry.topics.iter().all(|topic| self.topic_ready(topic));
                // Lost connections and an exhausted socket budget cannot indefinitely
                // suppress successful channel or personal metadata refreshes.
                (live || now.duration_since(entry.waiting_since) >= CONNECT_TIMEOUT)
                    .then(|| scope.clone())
            })
            .take(MAX_SNAPSHOTS.saturating_sub(self.jobs.len() + self.cosmetic_jobs.len()))
            .collect();
        for scope in due {
            let entry = self.watched.get_mut(&scope).unwrap();
            let Some(ticket) = self
                .index
                .read()
                .unwrap()
                .live_snapshot_ticket(&entry.target, entry.epoch)
            else {
                continue;
            };
            entry.in_flight = true;
            let target = entry.target.clone();
            let endpoint = self.endpoints.seven_tv_gql.clone();
            let client = self.client.clone();
            self.jobs.spawn(async move {
                let result = fetch_snapshot(&client, &endpoint, &target).await;
                Loaded {
                    target,
                    ticket,
                    result,
                }
            });
        }
        Ok(())
    }

    async fn complete_catalog(&mut self, loaded: CatalogLoad) -> Result<(), ()> {
        if let Some((badges, paints)) = loaded.result {
            self.cosmetic(
                0,
                SevenTvLiveCosmeticUpdate::CatalogRefreshFinished {
                    generation: loaded.generation,
                    badges,
                    paints,
                },
            )
            .await?;
        } else if !self.force_cosmetic_refresh {
            self.next_cosmetic_refresh = Instant::now() + RETRY_INTERVAL;
            self.force_cosmetic_refresh = true;
        }
        Ok(())
    }

    fn complete_snapshot(&mut self, loaded: Loaded) {
        let Some(entry) = self
            .watched
            .get_mut(&loaded.target.scope)
            .filter(|entry| entry.target == loaded.target && entry.epoch == loaded.ticket.epoch)
        else {
            return;
        };
        entry.in_flight = false;
        entry.next_snapshot = Instant::now()
            + if std::mem::take(&mut entry.refresh_after_load) {
                Duration::ZERO
            } else if loaded.result.is_some() {
                REFRESH_INTERVAL
            } else {
                RETRY_INTERVAL
            };
        let result = match loaded.result {
            Some(Snapshot::Emotes(emotes)) => self.index.write().unwrap().apply_live_snapshot(
                &loaded.target,
                loaded.ticket,
                emotes,
                true,
            ),
            Some(Snapshot::Source(source)) => super::reconcile::ApplyResult {
                changed: self.index.write().unwrap().switch_live_source(
                    &loaded.target,
                    entry.epoch,
                    source,
                ),
                needs_snapshot: false,
            },
            None => super::reconcile::ApplyResult::default(),
        };
        if result.needs_snapshot {
            entry.next_snapshot = Instant::now() + RETRY_INTERVAL;
        }
        if result.changed {
            let _ = self.dirty.try_send(());
        }
    }

    async fn apply_updates(
        &mut self,
        id: usize,
        epoch: u64,
        updates: Vec<seventv::Update>,
    ) -> Result<(), ()> {
        let owned = self.slots[&id].plan.topics.clone();
        let has_object = |event: &str, object: &str| {
            owned.iter().any(|topic| {
                topic.event == event && topic.condition == seventv::Condition::Object(object.into())
            })
        };
        let mut changed = false;
        for update in updates {
            match update {
                seventv::Update::Emotes { set_id, deltas }
                    if has_object("emote_set.update", &set_id) =>
                {
                    for entry in self
                        .watched
                        .values_mut()
                        .filter(|entry| entry.target.source.set_id.as_deref() == Some(&set_id))
                    {
                        for delta in &deltas {
                            let result = self.index.write().unwrap().apply_live_delta(
                                &entry.target,
                                entry.epoch,
                                delta.clone(),
                            );
                            changed |= result.changed;
                            if result.needs_snapshot {
                                entry.next_snapshot =
                                    entry.next_snapshot.min(Instant::now() + RETRY_INTERVAL);
                            }
                        }
                    }
                }
                seventv::Update::ActiveSet {
                    user_id,
                    connection_index,
                    set_id,
                } if has_object("user.update", &user_id) => {
                    for entry in self.watched.values().filter(|entry| {
                        entry.target.source.user_id.as_deref() == Some(&user_id)
                            && entry.target.source.connection_index == Some(connection_index)
                    }) {
                        let mut source = entry.target.source.clone();
                        source.set_id = set_id.clone();
                        changed |= self.index.write().unwrap().switch_live_source(
                            &entry.target,
                            entry.epoch,
                            source,
                        );
                    }
                }
                seventv::Update::DeletedSet(set_id) if has_object("emote_set.delete", &set_id) => {
                    for entry in self
                        .watched
                        .values()
                        .filter(|entry| entry.target.source.set_id.as_deref() == Some(&set_id))
                    {
                        let mut source = entry.target.source.clone();
                        source.set_id = None;
                        changed |= self.index.write().unwrap().switch_live_source(
                            &entry.target,
                            entry.epoch,
                            source,
                        );
                    }
                }
                seventv::Update::PersonalSetsChanged(user_id)
                    if has_object("user.update", &user_id) =>
                {
                    for entry in self.watched.values_mut().filter(|entry| {
                        matches!(entry.target.scope, LiveScope::Personal(_))
                            && entry.target.source.user_id.as_deref() == Some(&user_id)
                    }) {
                        entry.next_snapshot = Instant::now();
                        entry.refresh_after_load |= entry.in_flight;
                    }
                }
                seventv::Update::UserStyleChanged(user_id)
                    if has_object("user.update", &user_id) =>
                {
                    let mut twitch_user_ids: Vec<_> = self
                        .watched
                        .values()
                        .filter(|entry| {
                            entry.target.source.platform == "TWITCH"
                                && entry.target.source.user_id.as_deref() == Some(&user_id)
                        })
                        .map(|entry| entry.target.source.room_id.clone())
                        .collect();
                    twitch_user_ids.sort();
                    twitch_user_ids.dedup();
                    if !twitch_user_ids.is_empty() {
                        self.cosmetic(
                            epoch,
                            SevenTvLiveCosmeticUpdate::RefreshUsers { twitch_user_ids },
                        )
                        .await?;
                    }
                }
                seventv::Update::Cosmetic(update)
                    if owned
                        .iter()
                        .any(|topic| matches!(topic.condition, seventv::Condition::Channel(_))) =>
                {
                    self.cosmetic(epoch, update).await?;
                }
                _ => {}
            }
        }
        if changed {
            let _ = self.dirty.try_send(());
        }
        Ok(())
    }
}

async fn publish(sender: &mpsc::Sender<SocketEvent>, event: SocketEvent) -> bool {
    matches!(
        tokio::time::timeout(WRITE_TIMEOUT, sender.send(event)).await,
        Ok(Ok(()))
    )
}

async fn run_socket(
    id: usize,
    endpoint: String,
    events: mpsc::Sender<SocketEvent>,
    mut plans: watch::Receiver<SocketPlan>,
) {
    let mut attempts = 0u32;
    loop {
        if plans.has_changed().is_err() {
            return;
        }
        let revision = plans.borrow_and_update().revision;
        let config = WebSocketConfig {
            max_message_size: Some(1024 * 1024),
            max_frame_size: Some(1024 * 1024),
            ..WebSocketConfig::default()
        };
        let connecting = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async_with_config(&endpoint, Some(config), false),
        );
        tokio::pin!(connecting);
        let connection = loop {
            tokio::select! {
                changed = plans.changed() => {
                    if changed.is_err() { return; }
                    if plans.borrow_and_update().revision != revision { break None; }
                }
                result = &mut connecting => break Some(result),
            }
        };
        let mut immediate = connection.is_none();
        if let Some(Ok(Ok((mut socket, _)))) = connection {
            let epoch = next_epoch();
            let started = Instant::now();
            let mut hello = false;
            let mut topics = HashSet::new();
            let mut acknowledged = HashSet::new();
            let mut awaiting_since = None;
            let mut heartbeat = Duration::from_secs(30);
            let mut last_received = Instant::now();
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let plan = plans.borrow_and_update().clone();
                if plan.revision != revision {
                    immediate = true;
                    break;
                }
                if hello && plan.epoch == Some(epoch) && plan.topics != topics {
                    let mut failed = false;
                    for topic in topics.difference(&plan.topics) {
                        if write(&mut socket, Message::Text(topic.command(false).into()))
                            .await
                            .is_err()
                        {
                            failed = true;
                            break;
                        }
                    }
                    if failed {
                        break;
                    }
                    for topic in plan.topics.difference(&topics) {
                        if write(&mut socket, Message::Text(topic.command(true).into()))
                            .await
                            .is_err()
                        {
                            failed = true;
                            break;
                        }
                        awaiting_since.get_or_insert(Instant::now());
                    }
                    if failed {
                        break;
                    }
                    topics = plan.topics;
                    acknowledged.retain(|topic| topics.contains(topic));
                    if acknowledged == topics {
                        awaiting_since = None;
                    }
                }
                tokio::select! {
                    changed = plans.changed() => { if changed.is_err() { return; } }
                    incoming = socket.next() => {
                        let Some(Ok(message)) = incoming else { break; };
                        last_received = Instant::now();
                        match message {
                            Message::Ping(payload) => {
                                let (done, completed) = oneshot::channel();
                                if !publish(&events, SocketEvent::Barrier(done)).await
                                    || tokio::time::timeout(WRITE_TIMEOUT, completed).await.is_err()
                                {
                                    break;
                                }
                                if write(&mut socket, Message::Pong(payload)).await.is_err() { break; }
                            }
                            Message::Close(_) => break,
                            Message::Text(text) => {
                                let Some(frame) = seventv::decode(&text) else { continue; };
                                match frame {
                                    seventv::Frame::Hello { heartbeat: interval, subscription_limit } => {
                                        if hello { break; }
                                        hello = true;
                                        heartbeat = interval;
                                        if !publish(&events, SocketEvent::Hello { slot: id, revision, epoch, limit: subscription_limit }).await { break; }
                                    }
                                    seventv::Frame::Reconnect => { immediate = true; break; }
                                    seventv::Frame::Subscribed(ref topic) => {
                                        if topics.contains(topic) { acknowledged.insert(topic.clone()); }
                                        if acknowledged == topics { awaiting_since = None; }
                                        if !publish(&events, SocketEvent::Frame { slot: id, revision, epoch, frame }).await { break; }
                                    }
                                    seventv::Frame::Updates(_) => {
                                        if !publish(&events, SocketEvent::Frame { slot: id, revision, epoch, frame }).await { break; }
                                    }
                                    _ => {}
                                }
                            }
                            _ => {}
                        }
                    }
                    _ = tick.tick() => {
                        if !hello && started.elapsed() >= CONNECT_TIMEOUT { break; }
                        if awaiting_since.is_some_and(|at: Instant| at.elapsed() >= CONNECT_TIMEOUT) { break; }
                        if last_received.elapsed() >= heartbeat.saturating_mul(3) { break; }
                    }
                }
            }
            drop(socket);
            if !publish(
                &events,
                SocketEvent::Closed {
                    slot: id,
                    revision,
                    epoch,
                },
            )
            .await
            {
                return;
            }
            if started.elapsed() >= Duration::from_secs(30) {
                attempts = 0;
            }
        }
        if immediate {
            continue;
        }
        attempts = attempts.saturating_add(1);
        let backoff = Duration::from_secs(1u64 << attempts.saturating_sub(1).min(6));
        tokio::select! {
            changed = plans.changed() => { if changed.is_err() { return; } }
            _ = tokio::time::sleep(backoff) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::live_emotes::{LiveSource, Provider};
    use crust_core::model::ChannelId;

    fn update(code: &str) -> seventv::Frame {
        seventv::Frame::Updates(vec![seventv::Update::Emotes {
            set_id: "set".into(),
            deltas: vec![super::super::EmoteDelta::Upsert {
                emote: crust_emotes::EmoteInfo {
                    provider: "7tv".into(),
                    zero_width: false,
                    id: code.into(),
                    code: code.into(),
                    url_1x: format!("https://example.test/{code}"),
                    url_2x: None,
                    url_4x: None,
                },
                old_code: None,
                preserve_overlay: false,
            }],
        }])
    }

    fn fixture() -> (Pool, LiveTarget, ChannelId, u64, u64) {
        let index = crate::EmoteIndex::default();
        let channel = ChannelId::new("room");
        let target = {
            let mut registry = index.write().unwrap();
            registry.open_channel(&channel);
            let generation = registry.begin_channel_load(&channel, "42", false).unwrap();
            registry.replace_channel_with_source(
                &channel,
                generation,
                "7tv",
                Some(Vec::new()),
                Some(LiveSource {
                    provider: Provider::SevenTv,
                    room_id: "42".into(),
                    platform: "TWITCH".into(),
                    user_id: Some("user".into()),
                    set_id: Some("set".into()),
                    connection_index: Some(0),
                }),
                None,
            );
            registry.complete_channel_load(&channel, generation);
            registry.live_targets().remove(0)
        };
        let target_epoch = next_epoch();
        index
            .write()
            .unwrap()
            .begin_live_epoch(&target, target_epoch);
        let old_epoch = next_epoch();
        let revision = next_epoch();
        let topics: HashSet<_> = seventv::topics(&target).into_iter().collect();
        let plan = SocketPlan {
            revision,
            epoch: Some(old_epoch),
            topics: topics.clone(),
        };
        let (sender, _receiver) = watch::channel(plan.clone());
        let (events, _received) = mpsc::channel(4);
        let (dirty, _changed) = mpsc::channel(1);
        let pool = Pool {
            endpoints: Endpoints {
                socket: String::new(),
                bttv_api: String::new(),
                seven_tv_gql: String::new(),
            },
            client: reqwest::Client::new(),
            index: index.clone(),
            dirty,
            cosmetics: None,
            events,
            slots: BTreeMap::from([(
                0,
                Slot {
                    plan,
                    sender,
                    task: tokio::spawn(std::future::pending()),
                    capacity: Some(6),
                    acknowledged: topics,
                    retired: HashSet::new(),
                    last_epoch: old_epoch,
                },
            )]),
            watched: HashMap::from([(
                target.scope.clone(),
                Watched {
                    target: target.clone(),
                    topics: seventv::topics(&target),
                    epoch: target_epoch,
                    next_snapshot: Instant::now(),
                    in_flight: false,
                    refresh_after_load: false,
                    waiting_since: Instant::now(),
                    live: true,
                },
            )]),
            order: vec![target.scope.clone()],
            jobs: JoinSet::new(),
            cosmetic_jobs: JoinSet::new(),
            next_cosmetic_refresh: Instant::now(),
            force_cosmetic_refresh: false,
            cosmetic_recovery: HashSet::new(),
            reported_uncovered: 0,
        };
        (pool, target, channel, old_epoch, revision)
    }

    #[tokio::test]
    async fn completing_live_coverage_replaces_a_fallback_snapshot_before_the_periodic_refresh() {
        let (mut pool, target, _, epoch, revision) = fixture();
        let held = pool.watched[&target.scope].topics[0].clone();
        pool.slots.get_mut(&0).unwrap().acknowledged.remove(&held);
        let entry = pool.watched.get_mut(&target.scope).unwrap();
        entry.live = false;
        entry.in_flight = true;
        entry.waiting_since = Instant::now() - CONNECT_TIMEOUT;
        let ticket = pool
            .index
            .read()
            .unwrap()
            .live_snapshot_ticket(&target, entry.epoch)
            .unwrap();
        pool.on_socket_event(SocketEvent::Frame {
            slot: 0,
            revision,
            epoch,
            frame: seventv::Frame::Subscribed(held),
        })
        .await
        .unwrap();
        assert!(pool.watched[&target.scope].refresh_after_load);
        pool.complete_snapshot(Loaded {
            target: target.clone(),
            ticket,
            result: Some(Snapshot::Emotes(Vec::new())),
        });
        let entry = &pool.watched[&target.scope];
        assert!(entry.live);
        assert!(!entry.in_flight);
        assert!(
            entry.next_snapshot <= Instant::now(),
            "fallback completion postponed the first fully subscribed snapshot"
        );

        let held = pool.watched[&target.scope].topics[0].clone();
        pool.slots.get_mut(&0).unwrap().acknowledged.remove(&held);
        let entry = pool.watched.get_mut(&target.scope).unwrap();
        entry.live = false;
        entry.next_snapshot = Instant::now() + REFRESH_INTERVAL;
        pool.on_socket_event(SocketEvent::Frame {
            slot: 0,
            revision,
            epoch,
            frame: seventv::Frame::Subscribed(held),
        })
        .await
        .unwrap();
        assert!(
            pool.watched[&target.scope].next_snapshot <= Instant::now(),
            "an already completed fallback snapshot was reused after coverage became live"
        );

        // Reconnecting starts a new acknowledgment grace interval.
        let topics = pool.slots[&0].plan.topics.clone();
        pool.renew_scopes(&topics);
        assert!(pool.watched[&target.scope].waiting_since.elapsed() < CONNECT_TIMEOUT);
        assert!(!pool.watched[&target.scope].live);
    }

    #[tokio::test]
    async fn an_initial_fallback_catalog_is_rechecked_after_delayed_acknowledgments() {
        let (mut pool, target, _, epoch, revision) = fixture();
        let held = pool.watched[&target.scope].topics[0].clone();
        pool.slots.get_mut(&0).unwrap().acknowledged.remove(&held);
        let entry = pool.watched.get_mut(&target.scope).unwrap();
        entry.live = false;
        entry.in_flight = true;
        entry.waiting_since = Instant::now() - CONNECT_TIMEOUT;
        pool.schedule_snapshots().await.unwrap();
        assert_eq!(pool.cosmetic_jobs.len(), 1);
        assert!(pool.cosmetic_recovery.contains(&target.scope));
        pool.on_socket_event(SocketEvent::Frame {
            slot: 0,
            revision,
            epoch,
            frame: seventv::Frame::Subscribed(held),
        })
        .await
        .unwrap();
        pool.schedule_snapshots().await.unwrap();
        assert!(pool.force_cosmetic_refresh);
        let loaded = pool.cosmetic_jobs.join_next().await.unwrap().unwrap();
        pool.complete_catalog(loaded).await.unwrap();
        pool.schedule_snapshots().await.unwrap();
        assert_eq!(
            pool.cosmetic_jobs.len(),
            1,
            "the pre-ACK fallback prevented a fresh catalog after coverage became live"
        );
    }

    #[tokio::test]
    async fn restored_cosmetic_coverage_keeps_a_follow_up_due_after_an_older_request() {
        for succeeded in [true, false] {
            let (mut pool, target, _, epoch, revision) = fixture();
            let held = pool.watched[&target.scope]
                .topics
                .iter()
                .find(|topic| topic.event == "user.update")
                .unwrap()
                .clone();
            // A channel's user updates can use a different socket than its
            // cosmetic topics. Both paths need the same post-ACK refresh.
            pool.remember_cosmetic_gap(&HashSet::from([held.clone()]));
            assert!(pool.cosmetic_recovery.contains(&target.scope));
            pool.slots.get_mut(&0).unwrap().acknowledged.remove(&held);
            let entry = pool.watched.get_mut(&target.scope).unwrap();
            entry.live = false;
            entry.in_flight = true;
            pool.next_cosmetic_refresh = Instant::now() + REFRESH_INTERVAL;
            let (complete, pending) = oneshot::channel();
            pool.cosmetic_jobs
                .spawn(async move { pending.await.unwrap() });
            pool.schedule_snapshots().await.unwrap();
            assert!(!pool.force_cosmetic_refresh);

            pool.on_socket_event(SocketEvent::Frame {
                slot: 0,
                revision,
                epoch,
                frame: seventv::Frame::Subscribed(held),
            })
            .await
            .unwrap();
            pool.schedule_snapshots().await.unwrap();
            assert!(pool.force_cosmetic_refresh);
            assert_eq!(
                pool.cosmetic_jobs.len(),
                1,
                "recovery started a parallel catalog request"
            );
            assert!(pool.cosmetic_recovery.is_empty());
            complete
                .send(CatalogLoad {
                    generation: next_cosmetic_catalog_generation(),
                    result: succeeded.then(|| (HashMap::new(), HashMap::new())),
                })
                .unwrap_or_else(|_| panic!("catalog job stopped early"));
            let loaded = pool.cosmetic_jobs.join_next().await.unwrap().unwrap();
            pool.complete_catalog(loaded).await.unwrap();
            assert!(pool.force_cosmetic_refresh);
            assert!(
                pool.next_cosmetic_refresh <= Instant::now(),
                "an older request postponed the baseline after restored coverage"
            );
        }
    }

    #[tokio::test]
    async fn old_socket_epochs_revisions_and_snapshots_cannot_replace_current_emotes() {
        let (mut pool, target, channel, old_epoch, revision) = fixture();
        let index = pool.index.clone();
        let target_epoch = pool.watched[&target.scope].epoch;
        let has = |code: &str| index.read().unwrap().resolve(&channel, "", code).is_some();
        for (stale_revision, stale_epoch) in [(revision - 1, old_epoch), (revision, old_epoch - 1)]
        {
            pool.on_socket_event(SocketEvent::Frame {
                slot: 0,
                revision: stale_revision,
                epoch: stale_epoch,
                frame: update("Stale"),
            })
            .await
            .unwrap();
        }
        assert!(!has("Stale"));
        pool.on_socket_event(SocketEvent::Frame {
            slot: 0,
            revision,
            epoch: old_epoch,
            frame: update("Current"),
        })
        .await
        .unwrap();
        assert!(has("Current"));
        let old_ticket = index
            .read()
            .unwrap()
            .live_snapshot_ticket(&target, target_epoch)
            .unwrap();
        pool.on_socket_event(SocketEvent::Closed {
            slot: 0,
            revision,
            epoch: old_epoch,
        })
        .await
        .unwrap();
        pool.complete_snapshot(Loaded {
            target: target.clone(),
            ticket: old_ticket,
            result: Some(Snapshot::Emotes(Vec::new())),
        });
        assert!(
            has("Current"),
            "a disconnected snapshot cleared current emotes"
        );
        let replacement = next_epoch();
        pool.on_socket_event(SocketEvent::Hello {
            slot: 0,
            revision,
            epoch: replacement,
            limit: 6,
        })
        .await
        .unwrap();
        pool.on_socket_event(SocketEvent::Hello {
            slot: 0,
            revision,
            epoch: old_epoch,
            limit: 0,
        })
        .await
        .unwrap();
        pool.on_socket_event(SocketEvent::Closed {
            slot: 0,
            revision,
            epoch: old_epoch,
        })
        .await
        .unwrap();
        pool.on_socket_event(SocketEvent::Frame {
            slot: 0,
            revision,
            epoch: old_epoch,
            frame: update("Stale"),
        })
        .await
        .unwrap();
        assert_eq!(pool.slots[&0].plan.epoch, Some(replacement));
        assert!(!has("Stale"));
        pool.on_socket_event(SocketEvent::Frame {
            slot: 0,
            revision,
            epoch: replacement,
            frame: update("Replacement"),
        })
        .await
        .unwrap();
        assert!(has("Replacement"));
        pool.restart_slot(0).await.unwrap();
        pool.on_socket_event(SocketEvent::Frame {
            slot: 0,
            revision,
            epoch: replacement,
            frame: update("RetiredRevision"),
        })
        .await
        .unwrap();
        assert!(!has("RetiredRevision"));
    }
}

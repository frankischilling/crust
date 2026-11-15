use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crust_core::events::AppEvent;
use crust_core::model::ChannelId;
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinSet};

use super::badges::{publish_channel_badges, BadgeDefinitions, BadgeMap};

/// Snapshot jobs and their publications belong to one room connection.
#[derive(Default)]
pub(crate) struct RoomSnapshots {
    rooms: HashMap<ChannelId, RoomSnapshot>,
}

impl RoomSnapshots {
    pub(crate) fn begin(
        &mut self,
        channel: &ChannelId,
        room_id: &str,
        events: &mpsc::Sender<AppEvent>,
    ) -> Option<&mut RoomSnapshot> {
        if room_id.is_empty()
            || self
                .rooms
                .get(channel)
                .is_some_and(|room| room.room_id == room_id)
        {
            return None;
        }
        self.remove(channel);
        self.rooms.insert(
            channel.clone(),
            RoomSnapshot {
                room_id: room_id.to_owned(),
                initial: SnapshotJobs::new(events.clone()),
                shared: None,
            },
        );
        self.rooms.get_mut(channel)
    }

    pub(crate) fn get_mut(&mut self, channel: &ChannelId) -> Option<&mut RoomSnapshot> {
        self.rooms.get_mut(channel)
    }

    pub(crate) fn remove(&mut self, channel: &ChannelId) {
        self.rooms.remove(channel);
    }

    pub(crate) fn reconnect(&mut self) {
        self.rooms.clear();
    }
}

pub(crate) struct RoomSnapshot {
    room_id: String,
    initial: SnapshotJobs,
    shared: Option<SnapshotJobs>,
}

impl RoomSnapshot {
    pub(crate) fn events(&self) -> mpsc::Sender<AppEvent> {
        self.initial.events.clone()
    }

    pub(crate) fn spawn<F>(&mut self, load: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.initial.spawn(load);
    }

    /// A newer session probe supersedes both the old request and its queued result.
    pub(crate) fn spawn_shared<F, L>(&mut self, load: L)
    where
        F: Future<Output = ()> + Send + 'static,
        L: FnOnce(mpsc::Sender<AppEvent>) -> F,
    {
        self.shared.take();
        let mut jobs = SnapshotJobs::new(self.initial.destination.clone());
        jobs.spawn(load(jobs.events.clone()));
        self.shared = Some(jobs);
    }
}

struct SnapshotJobs {
    destination: mpsc::Sender<AppEvent>,
    events: mpsc::Sender<AppEvent>,
    current: Arc<Mutex<bool>>,
    tasks: JoinSet<()>,
}

impl SnapshotJobs {
    fn new(destination: mpsc::Sender<AppEvent>) -> Self {
        let (events, mut received) = mpsc::channel(1);
        let current = Arc::new(Mutex::new(true));
        let publish_current = current.clone();
        let publish_to = destination.clone();
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            while let Some(event) = received.recv().await {
                let Ok(permit) = publish_to.reserve().await else {
                    break;
                };
                // Check after backpressure clears. The lock also makes invalidation
                // and publication atomic; no lock is held across an await.
                let current = publish_current.lock().unwrap();
                if !*current {
                    break;
                }
                permit.send(event);
            }
        });
        Self {
            destination,
            events,
            current,
            tasks,
        }
    }

    fn spawn<F>(&mut self, load: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        while self.tasks.try_join_next().is_some() {}
        self.tasks.spawn(load);
    }
}

impl Drop for SnapshotJobs {
    fn drop(&mut self) {
        *self.current.lock().unwrap() = false;
        self.tasks.abort_all();
    }
}

enum BadgeStatus {
    Loading,
    Ready,
    RetryAfter(Instant),
}

struct BadgeLoad {
    room_id: String,
    generation: u64,
    status: BadgeStatus,
    abort: AbortHandle,
}

type BadgeResult = (ChannelId, u64, BadgeMap, Option<BadgeDefinitions>);
type BadgePublisher = fn(&str, &BadgeMap, BadgeDefinitions);

/// Badge failures can retry without repeating channel history requests.
pub(crate) struct BadgeRequests {
    channels: HashMap<ChannelId, BadgeLoad>,
    tasks: JoinSet<BadgeResult>,
    next_generation: u64,
    publish: BadgePublisher,
}

impl Default for BadgeRequests {
    fn default() -> Self {
        Self {
            channels: HashMap::new(),
            tasks: JoinSet::new(),
            next_generation: 0,
            publish: publish_channel_badges,
        }
    }
}

impl BadgeRequests {
    pub(crate) fn request<F>(
        &mut self,
        channel: &ChannelId,
        room_id: &str,
        force: bool,
        badge_map: &BadgeMap,
        load: F,
    ) -> bool
    where
        F: Future<Output = Option<BadgeDefinitions>> + Send + 'static,
    {
        self.request_at(channel, room_id, force, Instant::now(), badge_map, load)
    }

    fn request_at<F>(
        &mut self,
        channel: &ChannelId,
        room_id: &str,
        force: bool,
        now: Instant,
        badge_map: &BadgeMap,
        load: F,
    ) -> bool
    where
        F: Future<Output = Option<BadgeDefinitions>> + Send + 'static,
    {
        self.collect_finished(now);
        if room_id.is_empty() {
            return false;
        }
        if let Some(previous) = self.channels.get(channel) {
            if previous.room_id == room_id {
                match previous.status {
                    BadgeStatus::Loading => return false,
                    BadgeStatus::Ready if !force => return false,
                    BadgeStatus::RetryAfter(retry_at) if !force && now < retry_at => return false,
                    _ => {}
                }
            }
        }
        self.remove(channel);
        self.next_generation = self.next_generation.wrapping_add(1);
        let generation = self.next_generation;
        let loaded_channel = channel.clone();
        let badge_map = badge_map.clone();
        let abort = self
            .tasks
            .spawn(async move { (loaded_channel, generation, badge_map, load.await) });
        self.channels.insert(
            channel.clone(),
            BadgeLoad {
                room_id: room_id.to_owned(),
                generation,
                status: BadgeStatus::Loading,
                abort,
            },
        );
        true
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.tasks.is_empty()
    }

    /// Publish completed snapshots even when no further room updates arrive.
    pub(crate) async fn complete_next(&mut self) {
        if let Some(result) = self.tasks.join_next().await {
            self.apply_result(result, Instant::now());
        }
    }

    fn collect_finished(&mut self, now: Instant) {
        while let Some(result) = self.tasks.try_join_next() {
            self.apply_result(result, now);
        }
    }

    fn apply_result(&mut self, result: Result<BadgeResult, tokio::task::JoinError>, now: Instant) {
        match result {
            Ok((channel, generation, badge_map, definitions)) => {
                if let Some(entry) = self.channels.get_mut(&channel) {
                    if entry.generation == generation {
                        entry.status = if let Some(definitions) = definitions {
                            (self.publish)(channel.as_str(), &badge_map, definitions);
                            BadgeStatus::Ready
                        } else {
                            BadgeStatus::RetryAfter(now + Duration::from_secs(30))
                        };
                    }
                }
            }
            Err(error) => {
                if let Some(entry) = self
                    .channels
                    .values_mut()
                    .find(|entry| entry.abort.id() == error.id())
                {
                    entry.status = BadgeStatus::RetryAfter(now + Duration::from_secs(30));
                }
            }
        }
    }

    pub(crate) fn remove(&mut self, channel: &ChannelId) {
        if let Some(entry) = self.channels.remove(channel) {
            entry.abort.abort();
        }
    }

    pub(crate) fn clear(&mut self) {
        self.tasks.abort_all();
        self.channels.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (BadgeRequests, BadgeMap) {
        let requests = BadgeRequests {
            publish: |channel, map, definitions| {
                super::super::badges::replace_badge_scope(channel, map, definitions);
            },
            ..Default::default()
        };
        (requests, BadgeMap::default())
    }

    fn definitions(channel: &str, url: &str) -> BadgeDefinitions {
        HashMap::from([(
            (channel.into(), "subscriber".into(), "0".into()),
            url.into(),
        )])
    }

    async fn finish_one(requests: &mut BadgeRequests, now: Instant) {
        let result = tokio::time::timeout(Duration::from_secs(3), requests.tasks.join_next())
            .await
            .expect("badge task did not finish")
            .expect("expected a pending badge task");
        requests.apply_result(result, now);
    }

    #[tokio::test]
    async fn room_modes_reuse_snapshots_but_reconnect_rejoin_and_identity_changes_reload() {
        let channel = ChannelId::new("room");
        let (events, _received) = mpsc::channel(8);
        let mut snapshots = RoomSnapshots::default();
        assert!(snapshots.begin(&channel, "", &events).is_none());
        assert!(snapshots.begin(&channel, "123", &events).is_some());
        for _ in 0..5 {
            assert!(snapshots.begin(&channel, "123", &events).is_none());
        }
        snapshots.reconnect();
        assert!(snapshots.begin(&channel, "123", &events).is_some());
        snapshots.remove(&channel);
        assert!(snapshots.begin(&channel, "123", &events).is_some());
        assert!(snapshots.begin(&channel, "456", &events).is_some());
    }

    fn history(channel: &ChannelId, text: &str) -> AppEvent {
        AppEvent::HistoryLoaded {
            channel: channel.clone(),
            messages: vec![super::super::system_messages::make_system_message(
                1,
                channel.clone(),
                text.into(),
                chrono::Utc::now(),
                crust_core::model::MsgKind::SystemInfo,
            )],
            local: None,
        }
    }

    async fn wait_for_forwarder(events: &mpsc::Sender<AppEvent>) {
        // The mailbox has one slot. Reserving it after enqueueing an event proves
        // the forwarder consumed that event and reached the full destination.
        drop(
            tokio::time::timeout(Duration::from_secs(3), events.reserve())
                .await
                .unwrap()
                .unwrap(),
        );
    }

    #[tokio::test]
    async fn snapshot_validity_is_checked_after_event_backpressure_clears() {
        let channel = ChannelId::new("room");
        let (destination, mut received) = mpsc::channel(1);
        destination.send(AppEvent::LoggedOut).await.unwrap();
        let jobs = SnapshotJobs::new(destination);
        jobs.events
            .send(history(&channel, "stale history"))
            .await
            .unwrap();
        wait_for_forwarder(&jobs.events).await;

        // Leave the forwarder running to exercise the publication check itself,
        // independently of the task abort that normally accompanies invalidation.
        *jobs.current.lock().unwrap() = false;
        assert!(matches!(received.recv().await, Some(AppEvent::LoggedOut)));
        tokio::time::timeout(Duration::from_secs(3), jobs.events.closed())
            .await
            .unwrap();
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn leaving_cancels_jobs_and_old_history_cannot_reach_a_reopened_room() {
        let channel = ChannelId::new("room");
        let (destination, mut received) = mpsc::channel(1);
        destination.send(AppEvent::LoggedOut).await.unwrap();
        let mut snapshots = RoomSnapshots::default();
        let snapshot = snapshots.begin(&channel, "123", &destination).unwrap();
        let old_events = snapshot.events();
        let (mut completion, pending) = tokio::sync::oneshot::channel::<()>();
        snapshot.spawn(async move {
            let _ = pending.await;
        });
        old_events
            .send(history(&channel, "stale history"))
            .await
            .unwrap();
        wait_for_forwarder(&old_events).await;

        snapshots.remove(&channel);
        let events = snapshots
            .begin(&channel, "123", &destination)
            .unwrap()
            .events();
        events.send(history(&channel, "new history")).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), completion.closed())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), old_events.closed())
            .await
            .unwrap();
        assert!(matches!(received.recv().await, Some(AppEvent::LoggedOut)));
        let event = tokio::time::timeout(Duration::from_secs(3), received.recv())
            .await
            .unwrap()
            .unwrap();
        let AppEvent::HistoryLoaded { messages, .. } = event else {
            panic!("expected current history");
        };
        assert_eq!(messages[0].raw_text, "new history");
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn reconnect_cancels_every_room_and_replaced_identities_close_old_senders() {
        let (destination, _received) = mpsc::channel(1);
        let mut snapshots = RoomSnapshots::default();
        let mut old_events = Vec::new();
        let mut completions = Vec::new();
        for name in ["first", "second"] {
            let snapshot = snapshots
                .begin(&ChannelId::new(name), "123", &destination)
                .unwrap();
            old_events.push(snapshot.events());
            let (completion, pending) = tokio::sync::oneshot::channel::<()>();
            completions.push(completion);
            snapshot.spawn(async move {
                let _ = pending.await;
            });
        }
        snapshots.reconnect();
        for completion in &mut completions {
            tokio::time::timeout(Duration::from_secs(3), completion.closed())
                .await
                .unwrap();
        }
        for events in old_events {
            tokio::time::timeout(Duration::from_secs(3), events.closed())
                .await
                .unwrap();
        }
        let channel = ChannelId::new("first");
        let old_events = snapshots
            .begin(&channel, "123", &destination)
            .unwrap()
            .events();
        assert!(snapshots.begin(&channel, "456", &destination).is_some());
        tokio::time::timeout(Duration::from_secs(3), old_events.closed())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_new_shared_probe_supersedes_a_queued_session_without_cancelling_history() {
        let channel = ChannelId::new("room");
        let (destination, mut received) = mpsc::channel(1);
        destination.send(AppEvent::LoggedOut).await.unwrap();
        let mut snapshots = RoomSnapshots::default();
        let snapshot = snapshots.begin(&channel, "123", &destination).unwrap();
        snapshot.spawn_shared(|_| async {});
        let old_events = snapshot.shared.as_ref().unwrap().events.clone();
        old_events
            .send(AppEvent::SharedChatSessionUpdated {
                channel: channel.clone(),
                session: Some(crust_core::state::SharedChatSessionState {
                    session_id: "ended-session".into(),
                    host_broadcaster_id: "123".into(),
                    participants: Vec::new(),
                    updated_at: Instant::now(),
                }),
            })
            .await
            .unwrap();
        wait_for_forwarder(&old_events).await;
        let shared_channel = channel.clone();
        snapshot.spawn_shared(move |events| async move {
            events
                .send(AppEvent::SharedChatSessionUpdated {
                    channel: shared_channel,
                    session: None,
                })
                .await
                .unwrap();
        });
        tokio::time::timeout(Duration::from_secs(3), old_events.closed())
            .await
            .unwrap();
        assert!(matches!(received.recv().await, Some(AppEvent::LoggedOut)));
        let event = tokio::time::timeout(Duration::from_secs(3), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            AppEvent::SharedChatSessionUpdated { session: None, .. }
        ));

        snapshot
            .events()
            .send(history(&channel, "current history"))
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(3), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, AppEvent::HistoryLoaded { .. }));
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn badge_loads_coalesce_retry_failures_and_refresh_after_account_changes() {
        let channel = ChannelId::new("room");
        let (mut requests, map) = fixture();
        let now = Instant::now();
        let (finish, completed) = tokio::sync::oneshot::channel();
        assert!(
            requests.request_at(&channel, "123", false, now, &map, async {
                completed.await.unwrap()
            })
        );
        assert!(
            !requests.request_at(&channel, "123", false, now, &map, async {
                Some(HashMap::new())
            })
        );
        assert!(
            !requests.request_at(&channel, "123", true, now, &map, async {
                Some(HashMap::new())
            })
        );
        finish.send(None).unwrap();
        finish_one(&mut requests, now).await;
        assert!(matches!(
            requests.channels[&channel].status,
            BadgeStatus::RetryAfter(_)
        ));
        assert!(!requests.request_at(
            &channel,
            "123",
            false,
            now + Duration::from_secs(29),
            &map,
            async { Some(HashMap::new()) }
        ));
        assert!(requests.request_at(
            &channel,
            "123",
            false,
            now + Duration::from_secs(30),
            &map,
            async { Some(HashMap::new()) }
        ));
        finish_one(&mut requests, now + Duration::from_secs(30)).await;
        assert!(matches!(
            requests.channels[&channel].status,
            BadgeStatus::Ready
        ));
        assert!(!requests.request_at(
            &channel,
            "123",
            false,
            now + Duration::from_secs(90),
            &map,
            async { Some(HashMap::new()) }
        ));
        requests.clear();
        assert!(
            requests.request_at(&channel, "123", false, now, &map, async {
                Some(HashMap::new())
            })
        );
        finish_one(&mut requests, now).await;
        assert!(
            requests.request_at(&channel, "123", true, now, &map, async {
                Some(HashMap::new())
            })
        );
    }

    #[tokio::test]
    async fn completed_old_badge_snapshots_cannot_replace_a_new_room_identity() {
        let channel = ChannelId::new("room");
        let (mut requests, map) = fixture();
        assert!(requests.request(&channel, "old", false, &map, async {
            Some(definitions("room", "old-url"))
        }));
        let old_result = tokio::time::timeout(Duration::from_secs(3), requests.tasks.join_next())
            .await
            .unwrap()
            .unwrap();
        assert!(map.read().unwrap().is_empty());
        assert!(requests.request(&channel, "new", false, &map, async {
            Some(definitions("room", "new-url"))
        }));
        finish_one(&mut requests, Instant::now()).await;
        requests.apply_result(old_result, Instant::now());
        assert_eq!(requests.channels[&channel].room_id, "new");
        assert!(matches!(
            requests.channels[&channel].status,
            BadgeStatus::Ready
        ));
        assert_eq!(*map.read().unwrap(), definitions("room", "new-url"));
    }

    #[tokio::test]
    async fn a_panicked_badge_job_can_retry_after_the_failure_delay() {
        let channel = ChannelId::new("room");
        let (mut requests, map) = fixture();
        *map.write().unwrap() = definitions("room", "cached-url");
        let now = Instant::now();
        assert!(
            requests.request_at(&channel, "123", false, now, &map, async {
                panic!("simulated badge task failure");
            })
        );
        finish_one(&mut requests, now).await;
        assert!(matches!(
            requests.channels[&channel].status,
            BadgeStatus::RetryAfter(_)
        ));
        assert_eq!(*map.read().unwrap(), definitions("room", "cached-url"));
        assert!(!requests.request_at(
            &channel,
            "123",
            false,
            now + Duration::from_secs(29),
            &map,
            async { Some(HashMap::new()) }
        ));
        assert!(requests.request_at(
            &channel,
            "123",
            false,
            now + Duration::from_secs(30),
            &map,
            async { Some(definitions("room", "new-url")) }
        ));
        finish_one(&mut requests, now + Duration::from_secs(30)).await;
        assert!(matches!(
            requests.channels[&channel].status,
            BadgeStatus::Ready
        ));
        assert_eq!(*map.read().unwrap(), definitions("room", "new-url"));
    }

    #[tokio::test]
    async fn successful_empty_snapshot_removes_only_its_channel_and_failure_keeps_cache() {
        let channel = ChannelId::new("room");
        let (mut requests, map) = fixture();
        let mut original = definitions("room", "cached-url");
        original.extend(definitions("other", "other-url"));
        original.extend(definitions("", "global-url"));
        *map.write().unwrap() = original.clone();
        assert!(requests.request(&channel, "123", false, &map, async { None }));
        finish_one(&mut requests, Instant::now()).await;
        assert_eq!(*map.read().unwrap(), original);
        assert!(requests.request(&channel, "123", true, &map, async { Some(HashMap::new()) }));
        finish_one(&mut requests, Instant::now()).await;
        original.retain(|(scope, _, _), _| scope != "room");
        assert_eq!(*map.read().unwrap(), original);
        assert!(matches!(
            requests.channels[&channel].status,
            BadgeStatus::Ready
        ));
    }

    #[tokio::test]
    async fn cancelled_badge_jobs_cannot_publish_after_the_same_room_reopens() {
        let channel = ChannelId::new("room");
        let (mut requests, map) = fixture();
        let (finish, completed) = tokio::sync::oneshot::channel();
        assert!(requests.request(&channel, "123", false, &map, async {
            completed.await.unwrap_or(None)
        }));
        requests.remove(&channel);
        assert!(requests.request(&channel, "123", false, &map, async {
            Some(definitions("room", "reopened-url"))
        }));
        let _ = finish.send(Some(definitions("room", "cancelled-url")));
        while requests.has_pending() {
            finish_one(&mut requests, Instant::now()).await;
        }
        assert!(matches!(
            requests.channels[&channel].status,
            BadgeStatus::Ready
        ));
        assert_eq!(*map.read().unwrap(), definitions("room", "reopened-url"));
    }

    #[tokio::test]
    async fn badge_completion_publishes_without_another_room_event() {
        let channel = ChannelId::new("room");
        let (mut requests, map) = fixture();
        assert!(requests.request(&channel, "123", false, &map, async {
            Some(definitions("room", "loaded-url"))
        }));
        assert!(requests.has_pending());
        tokio::time::timeout(Duration::from_secs(3), requests.complete_next())
            .await
            .unwrap();
        assert!(!requests.has_pending());
        assert_eq!(*map.read().unwrap(), definitions("room", "loaded-url"));
    }
}

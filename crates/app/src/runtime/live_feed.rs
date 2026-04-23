use std::sync::Arc;
use std::time::{Duration, Instant};

use crust_core::model::LiveChannelSnapshot;
use crust_twitch::helix::{HelixApi, HelixError};
use futures_util::future::join_all;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{debug, warn};

/// How often the task polls `/helix/streams`.
pub const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// How long the followed-channel list is cached before refetching.
pub const FOLLOWED_CACHE_TTL: Duration = Duration::from_secs(600);

/// Helix `/streams` accepts at most 100 user_ids per call.
pub const STREAMS_BATCH_SIZE: usize = 100;

#[derive(Debug)]
pub enum LiveFeedCommand {
    /// Provide auth context and start polling.
    SetAuth { user_id: String },
    /// Drop auth context; emit an empty snapshot and idle.
    ClearAuth,
    /// Trigger an immediate poll without waiting for the next tick.
    ForceRefresh,
}

#[derive(Debug, Clone)]
pub enum LiveFeedEvent {
    Snapshot(Vec<LiveChannelSnapshot>),
    /// Partial snapshot: some batches succeeded, at least one failed. The
    /// successful channels are supplied along with the failure message so
    /// the UI can apply both in one atomic state update.
    PartialSnapshot {
        channels: Vec<LiveChannelSnapshot>,
        error: String,
    },
    Error(String),
}

/// Spawn the live-feed task. The `helix` argument is a trait object so tests
/// can substitute an in-memory double; production passes an `Arc<HelixClient>`.
#[allow(dead_code)] // Production uses `spawn_on`; tests use `spawn` under `#[tokio::test]`.
pub fn spawn(
    helix: Arc<dyn HelixApi>,
    cmd_rx: mpsc::Receiver<LiveFeedCommand>,
    evt_tx: mpsc::Sender<LiveFeedEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(helix, cmd_rx, evt_tx))
}

/// Spawn on an explicit tokio runtime handle. Use from sync code that runs
/// before a runtime is entered (e.g. `main()` before `rt.block_on`).
pub fn spawn_on(
    handle: &tokio::runtime::Handle,
    helix: Arc<dyn HelixApi>,
    cmd_rx: mpsc::Receiver<LiveFeedCommand>,
    evt_tx: mpsc::Sender<LiveFeedEvent>,
) -> tokio::task::JoinHandle<()> {
    handle.spawn(run(helix, cmd_rx, evt_tx))
}

struct State {
    user_id: Option<String>,
    followed: Vec<String>,
    followed_fetched_at: Option<Instant>,
}

impl State {
    fn new() -> Self {
        Self {
            user_id: None,
            followed: Vec::new(),
            followed_fetched_at: None,
        }
    }

    fn followed_is_stale(&self) -> bool {
        match self.followed_fetched_at {
            None => true,
            Some(t) => t.elapsed() >= FOLLOWED_CACHE_TTL,
        }
    }
}

async fn run(
    helix: Arc<dyn HelixApi>,
    mut cmd_rx: mpsc::Receiver<LiveFeedCommand>,
    evt_tx: mpsc::Sender<LiveFeedEvent>,
) {
    let mut state = State::new();
    loop {
        // Wait for either next tick or an inbound command, whichever fires first.
        // tokio::select! is not biased here; for the rare case where poll_once
        // outlasts POLL_INTERVAL the next iteration may immediately fire a
        // tick after a ForceRefresh. Acceptable: a single duplicate poll once
        // per ~15s window. join_all has reduced poll_once duration so this is
        // unlikely in practice.
        let next_action = if state.user_id.is_some() {
            tokio::select! {
                cmd = cmd_rx.recv() => cmd.map(NextAction::Command).unwrap_or(NextAction::Shutdown),
                _ = sleep(POLL_INTERVAL) => NextAction::Tick,
            }
        } else {
            match cmd_rx.recv().await {
                Some(c) => NextAction::Command(c),
                None => NextAction::Shutdown,
            }
        };

        match next_action {
            NextAction::Shutdown => break,
            NextAction::Command(LiveFeedCommand::SetAuth { user_id }) => {
                state.user_id = Some(user_id);
                state.followed.clear();
                state.followed_fetched_at = None;
                poll_once(&helix, &mut state, &evt_tx).await;
            }
            NextAction::Command(LiveFeedCommand::ClearAuth) => {
                state = State::new();
                let _ = evt_tx.send(LiveFeedEvent::Snapshot(Vec::new())).await;
            }
            NextAction::Command(LiveFeedCommand::ForceRefresh) => {
                if state.user_id.is_some() {
                    poll_once(&helix, &mut state, &evt_tx).await;
                }
            }
            NextAction::Tick => poll_once(&helix, &mut state, &evt_tx).await,
        }
    }
}

enum NextAction {
    Command(LiveFeedCommand),
    Tick,
    Shutdown,
}

async fn poll_once(
    helix: &Arc<dyn HelixApi>,
    state: &mut State,
    evt_tx: &mpsc::Sender<LiveFeedEvent>,
) {
    let user_id = match state.user_id.clone() {
        Some(u) => u,
        None => return,
    };

    if state.followed_is_stale() {
        match helix.get_followed(&user_id).await {
            Ok(list) => {
                state.followed = list.into_iter().map(|f| f.broadcaster_id).collect();
                state.followed_fetched_at = Some(Instant::now());
                debug!(
                    "live-feed: fetched {} followed channels",
                    state.followed.len()
                );
            }
            Err(e) => {
                let _ = evt_tx.send(LiveFeedEvent::Error(format_err(&e))).await;
                return;
            }
        }
    }

    if state.followed.is_empty() {
        let _ = evt_tx.send(LiveFeedEvent::Snapshot(Vec::new())).await;
        return;
    }

    // Issue concurrent stream queries for all chunks at once.
    let chunks: Vec<&[String]> = state.followed.chunks(STREAMS_BATCH_SIZE).collect();
    let futures = chunks.iter().map(|c| helix.get_streams(c));
    let results = join_all(futures).await;

    let mut all: Vec<LiveChannelSnapshot> = Vec::new();
    let mut first_err: Option<HelixError> = None;
    for r in results {
        match r {
            Ok(streams) => {
                for s in streams {
                    let raw_thumb = s.thumbnail_url.unwrap_or_default();
                    all.push(LiveChannelSnapshot {
                        user_id: s.user_id,
                        user_login: s.user_login,
                        user_name: s.user_name,
                        viewer_count: s.viewer_count,
                        thumbnail_url: LiveChannelSnapshot::template_thumbnail(
                            &raw_thumb, 320, 180,
                        ),
                        started_at: s.started_at.unwrap_or_default(),
                    });
                }
            }
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }

    if let Some(e) = first_err {
        warn!("live-feed: at least one streams batch failed: {e}");
        all.sort_by(|a, b| {
            b.viewer_count
                .cmp(&a.viewer_count)
                .then_with(|| a.started_at.cmp(&b.started_at))
        });
        let _ = evt_tx
            .send(LiveFeedEvent::PartialSnapshot {
                channels: all,
                error: format_err(&e),
            })
            .await;
        return;
    }

    // Sort desc by viewer_count, tiebreak by started_at asc (older first).
    all.sort_by(|a, b| {
        b.viewer_count
            .cmp(&a.viewer_count)
            .then_with(|| a.started_at.cmp(&b.started_at))
    });
    let _ = evt_tx.send(LiveFeedEvent::Snapshot(all)).await;
}

fn format_err(e: &HelixError) -> String {
    match e {
        HelixError::MissingScope(s) => {
            format!("Missing OAuth scope: {s}. Re-login with the scope to enable Live feed.")
        }
        HelixError::NotAuthenticated => "Live feed: not logged in.".to_owned(),
        HelixError::RateLimited => "Rate limited by Twitch (will retry)".to_owned(),
        HelixError::Http(m) => format!("Network error: {m}"),
        HelixError::Decode(m) => format!("Decode error: {m}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use crust_twitch::helix::{FollowedChannel, HelixStream};
    use std::sync::Mutex;
    use tokio::time::timeout;

    /// In-memory `HelixApi` double.
    struct FakeHelix {
        followed: Mutex<Result<Vec<FollowedChannel>, HelixError>>,
        streams: Mutex<Result<Vec<HelixStream>, HelixError>>,
    }

    impl FakeHelix {
        fn new(followed: Vec<FollowedChannel>, streams: Vec<HelixStream>) -> Arc<Self> {
            Arc::new(Self {
                followed: Mutex::new(Ok(followed)),
                streams: Mutex::new(Ok(streams)),
            })
        }
    }

    #[async_trait]
    impl HelixApi for FakeHelix {
        async fn get_followed(&self, _user_id: &str) -> Result<Vec<FollowedChannel>, HelixError> {
            self.followed.lock().unwrap().clone()
        }
        async fn get_streams(&self, _ids: &[String]) -> Result<Vec<HelixStream>, HelixError> {
            self.streams.lock().unwrap().clone()
        }
    }

    fn fc(id: &str) -> FollowedChannel {
        FollowedChannel {
            broadcaster_id: id.into(),
            broadcaster_login: format!("user{id}"),
        }
    }
    fn hs(login: &str, viewers: u32, started: &str) -> HelixStream {
        HelixStream {
            user_id: format!("id-{login}"),
            user_login: login.into(),
            user_name: login.into(),
            viewer_count: viewers,
            thumbnail_url: Some("https://x/{width}x{height}.jpg".into()),
            started_at: Some(started.into()),
        }
    }

    async fn next_event(rx: &mut mpsc::Receiver<LiveFeedEvent>) -> LiveFeedEvent {
        timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("event timeout")
            .expect("channel closed")
    }

    #[tokio::test]
    async fn set_auth_emits_first_snapshot_sorted_desc_by_viewers() {
        let helix = FakeHelix::new(
            vec![fc("1"), fc("2")],
            vec![
                hs("a", 10, "2026-04-22T10:00:00Z"),
                hs("b", 50, "2026-04-22T09:00:00Z"),
            ],
        );
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (evt_tx, mut evt_rx) = mpsc::channel(8);
        let _h = spawn(helix, cmd_rx, evt_tx);

        cmd_tx
            .send(LiveFeedCommand::SetAuth {
                user_id: "self".into(),
            })
            .await
            .unwrap();

        let evt = next_event(&mut evt_rx).await;
        match evt {
            LiveFeedEvent::Snapshot(s) => {
                assert_eq!(s.len(), 2);
                assert_eq!(s[0].user_login, "b"); // 50 viewers first
                assert_eq!(s[1].user_login, "a");
                assert_eq!(s[0].thumbnail_url, "https://x/320x180.jpg");
            }
            other => panic!("expected Snapshot, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn followed_unauthorized_emits_error_no_snapshot() {
        struct H;
        #[async_trait]
        impl HelixApi for H {
            async fn get_followed(&self, _u: &str) -> Result<Vec<FollowedChannel>, HelixError> {
                Err(HelixError::MissingScope("user:read:follows"))
            }
            async fn get_streams(&self, _i: &[String]) -> Result<Vec<HelixStream>, HelixError> {
                Ok(Vec::new())
            }
        }
        let helix: Arc<dyn HelixApi> = Arc::new(H);
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (evt_tx, mut evt_rx) = mpsc::channel(8);
        let _h = spawn(helix, cmd_rx, evt_tx);

        cmd_tx
            .send(LiveFeedCommand::SetAuth {
                user_id: "self".into(),
            })
            .await
            .unwrap();

        match next_event(&mut evt_rx).await {
            LiveFeedEvent::Error(m) => assert!(m.contains("user:read:follows")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_followed_list_emits_empty_snapshot() {
        let helix = FakeHelix::new(Vec::new(), Vec::new());
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (evt_tx, mut evt_rx) = mpsc::channel(8);
        let _h = spawn(helix, cmd_rx, evt_tx);

        cmd_tx
            .send(LiveFeedCommand::SetAuth {
                user_id: "self".into(),
            })
            .await
            .unwrap();

        match next_event(&mut evt_rx).await {
            LiveFeedEvent::Snapshot(s) => assert!(s.is_empty()),
            other => panic!("expected empty Snapshot, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn force_refresh_emits_extra_snapshot_before_tick() {
        let helix = FakeHelix::new(vec![fc("1")], vec![hs("a", 10, "2026-04-22T10:00:00Z")]);
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (evt_tx, mut evt_rx) = mpsc::channel(8);
        let _h = spawn(helix, cmd_rx, evt_tx);

        cmd_tx
            .send(LiveFeedCommand::SetAuth {
                user_id: "self".into(),
            })
            .await
            .unwrap();
        let _ = next_event(&mut evt_rx).await; // initial snapshot

        cmd_tx.send(LiveFeedCommand::ForceRefresh).await.unwrap();
        // Must arrive well before the 15 s tick.
        let evt = timeout(Duration::from_millis(500), evt_rx.recv())
            .await
            .expect("ForceRefresh did not produce snapshot in <500 ms")
            .expect("channel closed");
        assert!(matches!(evt, LiveFeedEvent::Snapshot(_)));
    }

    #[tokio::test]
    async fn clear_auth_emits_empty_snapshot_then_idles() {
        let helix = FakeHelix::new(vec![fc("1")], vec![hs("a", 10, "2026-04-22T10:00:00Z")]);
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (evt_tx, mut evt_rx) = mpsc::channel(8);
        let _h = spawn(helix, cmd_rx, evt_tx);

        cmd_tx
            .send(LiveFeedCommand::SetAuth {
                user_id: "self".into(),
            })
            .await
            .unwrap();
        let _ = next_event(&mut evt_rx).await;

        cmd_tx.send(LiveFeedCommand::ClearAuth).await.unwrap();
        match next_event(&mut evt_rx).await {
            LiveFeedEvent::Snapshot(s) => assert!(s.is_empty()),
            other => panic!("expected empty Snapshot from ClearAuth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn followed_cache_is_reused_within_ttl() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Counting {
            followed_calls: AtomicUsize,
            streams_calls: AtomicUsize,
        }
        #[async_trait]
        impl HelixApi for Counting {
            async fn get_followed(&self, _u: &str) -> Result<Vec<FollowedChannel>, HelixError> {
                self.followed_calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![FollowedChannel {
                    broadcaster_id: "1".into(),
                    broadcaster_login: "x".into(),
                }])
            }
            async fn get_streams(&self, _i: &[String]) -> Result<Vec<HelixStream>, HelixError> {
                self.streams_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            }
        }

        let helix = Arc::new(Counting {
            followed_calls: AtomicUsize::new(0),
            streams_calls: AtomicUsize::new(0),
        });
        let helix_clone = helix.clone();
        let helix_dyn: Arc<dyn HelixApi> = helix;
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (evt_tx, mut evt_rx) = mpsc::channel(8);
        let _h = spawn(helix_dyn, cmd_rx, evt_tx);

        cmd_tx
            .send(LiveFeedCommand::SetAuth {
                user_id: "self".into(),
            })
            .await
            .unwrap();
        let _ = next_event(&mut evt_rx).await;

        cmd_tx.send(LiveFeedCommand::ForceRefresh).await.unwrap();
        let _ = next_event(&mut evt_rx).await;

        cmd_tx.send(LiveFeedCommand::ForceRefresh).await.unwrap();
        let _ = next_event(&mut evt_rx).await;

        // 3 polls (SetAuth + 2 ForceRefresh) but only one followed call,
        // because the cache is fresh within the 10-min TTL.
        assert_eq!(helix_clone.followed_calls.load(Ordering::SeqCst), 1);
        assert!(helix_clone.streams_calls.load(Ordering::SeqCst) >= 3);
    }
}

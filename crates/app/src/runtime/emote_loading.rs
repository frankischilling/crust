use crust_core::{events::AppEvent, model::ChannelId};
use crust_emotes::{
    providers::{
        BttvProvider, EmoteInfo, FfzProvider, KickProvider, SevenTvProvider, TwitchGlobalProvider,
    },
    EmoteProvider,
};
use futures_util::{future::BoxFuture, stream::FuturesUnordered, FutureExt, StreamExt};
use tokio::sync::mpsc;

use super::live_emotes::{LiveBinding, LiveSource, Provider};
use crate::seventv::{load_7tv_user_for_twitch, SevenTvCosmeticUpdate};

type ProviderLoad<'a> = BoxFuture<'a, (&'static str, Option<Vec<EmoteInfo>>)>;
type ChannelProviderLoad<'a> = BoxFuture<'a, ChannelProviderResult>;

const MAX_CHANNEL_LOADS: usize = 4;
static CHANNEL_LOADS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_CHANNEL_LOADS);

struct ChannelProviderResult {
    provider: &'static str,
    emotes: Option<Vec<EmoteInfo>>,
    source: Option<LiveSource>,
    expected_source: Option<LiveBinding>,
}

impl From<(&'static str, Option<Vec<EmoteInfo>>)> for ChannelProviderResult {
    fn from((provider, emotes): (&'static str, Option<Vec<EmoteInfo>>)) -> Self {
        Self {
            provider,
            emotes,
            source: None,
            expected_source: None,
        }
    }
}

async fn load_seven_tv_channel(
    provider: &SevenTvProvider,
    platform: &str,
    room_id: &str,
    expected_source: Option<LiveBinding>,
) -> ChannelProviderResult {
    match provider.load_channel_state(platform, room_id).await {
        Some(state) => ChannelProviderResult {
            provider: "7tv",
            emotes: Some(state.emotes),
            source: Some(LiveSource {
                provider: Provider::SevenTv,
                platform: platform.into(),
                room_id: room_id.into(),
                user_id: state.user_id,
                set_id: state.set_id,
                connection_index: state.connection_index,
            }),
            expected_source,
        },
        None => ChannelProviderResult {
            provider: "7tv",
            emotes: None,
            source: None,
            expected_source,
        },
    }
}

/// Publish built-in globals immediately and load remote provider sets in the background.
pub(crate) async fn load_global_emotes(index: &crate::EmoteIndex, evt_tx: &mpsc::Sender<AppEvent>) {
    static LOADS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    let Ok(permit) = LOADS.try_acquire() else {
        return;
    };
    let twitch = TwitchGlobalProvider::new().load_global().await;
    let (generation, expected_source, changed) = {
        let mut registry = index.write().unwrap();
        let generation = registry.begin_global_load();
        let changed = registry.replace_global_result(generation, "twitch", Some(twitch));
        (generation, registry.global_live_binding(), changed)
    };
    if changed {
        send_emote_catalog(index, evt_tx).await;
    }

    let bttv = BttvProvider::new();
    let ffz = FfzProvider::new();
    let seven_tv = SevenTvProvider::new();
    let index = index.clone();
    let evt_tx = evt_tx.clone();
    tokio::spawn(async move {
        let _permit = permit;
        let providers: FuturesUnordered<ProviderLoad<'_>> = [
            async { ("bttv", bttv.try_load_global().await) }.boxed(),
            async { ("ffz", ffz.try_load_global().await) }.boxed(),
        ]
        .into_iter()
        .collect();
        tokio::join!(
            publish_global_loads(generation, providers, &index, &evt_tx),
            async {
                let Some(set) = seven_tv.load_global_state().await else {
                    return;
                };
                let changed = {
                    let mut registry = index.write().unwrap();
                    registry.is_current_global_load(generation)
                        && registry.replace_global_live(
                            LiveSource {
                                provider: Provider::SevenTv,
                                room_id: "global".into(),
                                platform: "GLOBAL".into(),
                                user_id: None,
                                set_id: Some(set.id),
                                connection_index: None,
                            },
                            set.emotes,
                            expected_source,
                        )
                };
                if changed {
                    send_emote_catalog(&index, &evt_tx).await;
                }
            }
        );
    });
}

/// Refresh global metadata without overlapping manual loads or requesting images.
pub(crate) fn spawn_global_refresh(
    index: &crate::EmoteIndex,
    evt_tx: &mpsc::Sender<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    let index = index.clone();
    let evt_tx = evt_tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = evt_tx.closed() => break,
                _ = tokio::time::sleep(std::time::Duration::from_secs(5 * 60)) => {
                    load_global_emotes(&index, &evt_tx).await;
                }
            }
        }
    })
}

async fn publish_global_loads(
    generation: u64,
    mut providers: FuturesUnordered<ProviderLoad<'_>>,
    index: &crate::EmoteIndex,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    while let Some((provider, emotes)) = providers.next().await {
        if evt_tx.is_closed() {
            return;
        }
        let changed = index
            .write()
            .unwrap()
            .replace_global_result(generation, provider, emotes);
        if changed {
            send_emote_catalog(index, evt_tx).await;
        }
    }
}

pub(crate) fn queue_7tv_user_lookup(
    user_id: &str,
    channel: Option<&ChannelId>,
    index: &crate::EmoteIndex,
    client: &reqwest::Client,
    semaphore: &std::sync::Arc<tokio::sync::Semaphore>,
    update_tx: &mpsc::Sender<SevenTvCosmeticUpdate>,
) -> bool {
    // This permit also counts obsolete jobs until they finish, so repeated
    // account changes cannot grow an unbounded queue behind the HTTP semaphore.
    static QUEUED_USERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(64);
    let Ok(job) = QUEUED_USERS.try_acquire() else {
        index.write().unwrap().defer_personal_load(user_id, channel);
        return false;
    };
    let generation = index.write().unwrap().begin_personal_load(user_id, channel);
    let Some(generation) = generation else {
        index.write().unwrap().defer_personal_load(user_id, channel);
        return false;
    };
    let user_id = user_id.to_owned();
    let client = client.clone();
    let semaphore = semaphore.clone();
    let update_tx = update_tx.clone();
    tokio::spawn(async move {
        let _job = job;
        let Ok(_permit) = semaphore.acquire().await else {
            return;
        };
        let profile = load_7tv_user_for_twitch(&client, &user_id).await;
        let _ = update_tx
            .send(SevenTvCosmeticUpdate::UserStyle {
                twitch_user_id: user_id,
                generation,
                profile,
            })
            .await;
    });
    true
}

pub(crate) fn drain_7tv_user_lookups(
    index: &crate::EmoteIndex,
    client: &reqwest::Client,
    semaphore: &std::sync::Arc<tokio::sync::Semaphore>,
    update_tx: &mpsc::Sender<SevenTvCosmeticUpdate>,
) {
    let active = index.read().unwrap().active_user_needing_lookup();
    if let Some(user) = active {
        queue_7tv_user_lookup(&user, None, index, client, semaphore, update_tx);
    }
    for _ in 0..64 {
        let request = index.write().unwrap().next_deferred_personal_load();
        let Some((user, channel)) = request else {
            break;
        };
        if !queue_7tv_user_lookup(&user, channel.as_ref(), index, client, semaphore, update_tx) {
            break;
        }
    }
}

pub(crate) async fn load_channel_emotes(
    channel: &ChannelId,
    room_id: &str,
    generation: u64,
    index: &crate::EmoteIndex,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    load_selected_channel_emotes(channel, room_id, generation, index, evt_tx, None).await;
}

async fn load_selected_channel_emotes(
    channel: &ChannelId,
    room_id: &str,
    generation: u64,
    index: &crate::EmoteIndex,
    evt_tx: &mpsc::Sender<AppEvent>,
    selected: Option<&[String]>,
) {
    let includes =
        |provider: &str| selected.is_none_or(|names| names.iter().any(|name| name == provider));
    let bttv = BttvProvider::new();
    let ffz = FfzProvider::new();
    let seven_tv = SevenTvProvider::new();
    let bttv_source = index
        .read()
        .unwrap()
        .live_binding(channel, generation, "bttv");
    let seven_tv_source = index
        .read()
        .unwrap()
        .live_binding(channel, generation, "7tv");
    let providers: FuturesUnordered<ChannelProviderLoad<'_>> = FuturesUnordered::new();
    if includes("bttv") {
        providers.push(
            async {
                ChannelProviderResult {
                    provider: "bttv",
                    emotes: bttv.load_channel(room_id).await,
                    source: Some(LiveSource {
                        provider: Provider::Bttv,
                        platform: "TWITCH".into(),
                        room_id: room_id.into(),
                        user_id: None,
                        set_id: None,
                        connection_index: None,
                    }),
                    expected_source: bttv_source,
                }
            }
            .boxed(),
        );
    }
    if includes("ffz") {
        providers.push(
            async { ChannelProviderResult::from(("ffz", ffz.load_channel(room_id).await)) }.boxed(),
        );
    }
    if includes("7tv") {
        providers
            .push(load_seven_tv_channel(&seven_tv, "TWITCH", room_id, seven_tv_source).boxed());
    }
    publish_channel_loads(channel, generation, providers, index, evt_tx).await;
}

pub(crate) async fn load_kick_channel_emotes(
    channel: &ChannelId,
    kick_user_id: u64,
    generation: u64,
    index: &crate::EmoteIndex,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    load_selected_kick_channel_emotes(channel, kick_user_id, generation, index, evt_tx, None).await;
}

async fn load_selected_kick_channel_emotes(
    channel: &ChannelId,
    kick_user_id: u64,
    generation: u64,
    index: &crate::EmoteIndex,
    evt_tx: &mpsc::Sender<AppEvent>,
    selected: Option<&[String]>,
) {
    let includes =
        |provider: &str| selected.is_none_or(|names| names.iter().any(|name| name == provider));
    let kick = KickProvider::new();
    let seven_tv = SevenTvProvider::new();
    let seven_tv_source = index
        .read()
        .unwrap()
        .live_binding(channel, generation, "7tv");
    let room_id = kick_user_id.to_string();
    let providers: FuturesUnordered<ChannelProviderLoad<'_>> = FuturesUnordered::new();
    if includes("kick") {
        providers.push(
            async {
                ChannelProviderResult::from((
                    "kick",
                    kick.load_channel(channel.display_name()).await,
                ))
            }
            .boxed(),
        );
    }
    if includes("7tv") {
        providers.push(
            async {
                if kick_user_id == 0 {
                    ChannelProviderResult::from(("7tv", Some(Vec::new())))
                } else {
                    load_seven_tv_channel(&seven_tv, "KICK", &room_id, seven_tv_source).await
                }
            }
            .boxed(),
        );
    }
    publish_channel_loads(channel, generation, providers, index, evt_tx).await;
}

/// Retry failed channel metadata without waiting for another room-state event.
pub(crate) fn retry_failed_channel_emotes(
    index: &crate::EmoteIndex,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    if evt_tx.is_closed() {
        return;
    }
    let retries = index
        .write()
        .unwrap()
        .take_channel_emote_retries(std::time::Instant::now(), MAX_CHANNEL_LOADS);
    for retry in retries {
        let index = index.clone();
        let evt_tx = evt_tx.clone();
        tokio::spawn(async move {
            if retry.channel.is_twitch() {
                load_selected_channel_emotes(
                    &retry.channel,
                    &retry.room_id,
                    retry.generation,
                    &index,
                    &evt_tx,
                    Some(&retry.providers),
                )
                .await;
            } else if let Ok(user_id) = retry.room_id.parse() {
                load_selected_kick_channel_emotes(
                    &retry.channel,
                    user_id,
                    retry.generation,
                    &index,
                    &evt_tx,
                    Some(&retry.providers),
                )
                .await;
            }
        });
    }
}

async fn publish_channel_loads<F, R>(
    channel: &ChannelId,
    generation: u64,
    providers: FuturesUnordered<F>,
    index: &crate::EmoteIndex,
    evt_tx: &mpsc::Sender<AppEvent>,
) where
    F: std::future::Future<Output = R>,
    R: Into<ChannelProviderResult>,
{
    publish_channel_loads_with_limit(
        channel,
        generation,
        providers,
        index,
        evt_tx,
        &CHANNEL_LOADS,
    )
    .await;
}

async fn publish_channel_loads_with_limit<F, R>(
    channel: &ChannelId,
    generation: u64,
    mut providers: FuturesUnordered<F>,
    index: &crate::EmoteIndex,
    evt_tx: &mpsc::Sender<AppEvent>,
    limit: &tokio::sync::Semaphore,
) where
    F: std::future::Future<Output = R>,
    R: Into<ChannelProviderResult>,
{
    if evt_tx.is_closed()
        || !index
            .read()
            .unwrap()
            .is_current_channel_load(channel, generation)
    {
        return;
    }
    let Ok(_permit) = (tokio::select! {
        _ = evt_tx.closed() => return,
        permit = limit.acquire() => permit,
    }) else {
        return;
    };
    // Provider futures have not been polled yet. Departed or superseded scopes
    // leave the queue without starting network requests.
    if evt_tx.is_closed()
        || !index
            .read()
            .unwrap()
            .is_current_channel_load(channel, generation)
    {
        return;
    }
    // Each completed provider becomes available immediately. A slow response
    // cannot hold the other catalogs back, and images load when a widget uses them.
    while let Some(result) = providers.next().await {
        let ChannelProviderResult {
            provider,
            emotes,
            source,
            expected_source,
        } = result.into();
        let succeeded = emotes.is_some();
        if evt_tx.is_closed()
            || !index.write().unwrap().replace_channel_with_source(
                channel,
                generation,
                provider,
                emotes,
                source,
                expected_source,
            )
        {
            return;
        }
        if succeeded {
            send_emote_catalog(index, evt_tx).await;
        }
    }

    // Reserve capacity before checking generation: the channel can close or
    // start a replacement load while the event queue is full.
    let Ok(permit) = evt_tx.reserve().await else {
        return;
    };
    let mut registry = index.write().unwrap();
    if registry.complete_channel_load(channel, generation) {
        permit.send(AppEvent::ChannelEmotesLoaded {
            channel: channel.clone(),
            count: registry.channel_emote_count(channel),
        });
    }
}

/// Serialize publications so a slow sender cannot publish an obsolete snapshot last.
pub(crate) async fn send_emote_catalog(index: &crate::EmoteIndex, evt_tx: &mpsc::Sender<AppEvent>) {
    static PUBLISH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _publication = PUBLISH.lock().await;
    let Ok(permit) = evt_tx.reserve().await else {
        return;
    };
    let registry = index.read().unwrap();
    permit.send(AppEvent::EmoteCatalogUpdated {
        emotes: registry.catalog(),
    });
}

pub(crate) async fn clear_personal_emotes(
    index: &crate::EmoteIndex,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    index.write().unwrap().set_account(None);
    send_emote_catalog(index, evt_tx).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, RwLock};
    use std::task::Poll;
    use std::time::Duration;

    fn index() -> crate::EmoteIndex {
        Arc::new(RwLock::new(
            super::super::emote_registry::EmoteRegistry::default(),
        ))
    }

    fn emote(provider: &str, code: &str) -> EmoteInfo {
        EmoteInfo {
            provider: provider.to_owned(),
            zero_width: false,
            id: code.to_owned(),
            code: code.to_owned(),
            url_1x: format!("https://example.test/{code}.png"),
            url_2x: None,
            url_4x: None,
        }
    }

    async fn receive(events: &mut mpsc::Receiver<AppEvent>) -> AppEvent {
        tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .expect("provider publication timed out")
            .expect("provider publication closed")
    }

    fn codes(event: AppEvent) -> Vec<String> {
        let AppEvent::EmoteCatalogUpdated { emotes } = event else {
            panic!("expected a catalog publication");
        };
        let mut codes: Vec<_> = emotes.into_iter().map(|emote| emote.code).collect();
        codes.sort();
        codes
    }

    #[tokio::test]
    async fn global_catalog_publishes_fast_providers_before_slow_ones() {
        let index = index();
        let (events, mut received) = mpsc::channel(4);
        let (release, pending) = tokio::sync::oneshot::channel();
        let providers: FuturesUnordered<ProviderLoad<'static>> = [
            async { ("bttv", Some(vec![emote("bttv", "Fast")])) }.boxed(),
            async move {
                pending.await.unwrap();
                ("7tv", Some(vec![emote("7tv", "Slow")]))
            }
            .boxed(),
        ]
        .into_iter()
        .collect();
        let task = tokio::spawn(async move {
            let generation = index.write().unwrap().begin_global_load();
            publish_global_loads(generation, providers, &index, &events).await;
        });

        assert_eq!(codes(receive(&mut received).await), ["Fast"]);
        assert!(!task.is_finished());
        release.send(()).unwrap();
        assert_eq!(codes(receive(&mut received).await), ["Fast", "Slow"]);
        task.await.unwrap();
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn channel_catalog_updates_progressively_and_settles_once() {
        let index = index();
        let channel = ChannelId::new("room");
        let generation = {
            let mut registry = index.write().unwrap();
            registry.open_channel(&channel);
            let generation = registry.begin_channel_load(&channel, "42", false).unwrap();
            registry.replace_channel(
                &channel,
                generation,
                "7tv",
                Some(vec![emote("7tv", "Retained")]),
            );
            generation
        };
        let (events, mut received) = mpsc::channel(4);
        let (release, pending) = tokio::sync::oneshot::channel();
        let providers: FuturesUnordered<ProviderLoad<'static>> = [
            async { ("bttv", Some(vec![emote("bttv", "Fast")])) }.boxed(),
            async move {
                pending.await.unwrap();
                ("ffz", Some(vec![emote("ffz", "Slow")]))
            }
            .boxed(),
            async { ("7tv", None) }.boxed(),
        ]
        .into_iter()
        .collect();
        let task_channel = channel.clone();
        let task_index = index.clone();
        let task = tokio::spawn(async move {
            publish_channel_loads(&task_channel, generation, providers, &task_index, &events).await;
        });

        assert_eq!(codes(receive(&mut received).await), ["Fast", "Retained"]);
        assert!(
            received.try_recv().is_err(),
            "a pending provider must not complete the channel"
        );
        release.send(()).unwrap();
        assert_eq!(
            codes(receive(&mut received).await),
            ["Fast", "Retained", "Slow"]
        );
        assert!(matches!(receive(&mut received).await,
            AppEvent::ChannelEmotesLoaded { channel: loaded, count: 3 } if loaded == channel));
        task.await.unwrap();
        assert!(received.try_recv().is_err());
        let mut registry = index.write().unwrap();
        assert!(
            registry.begin_channel_load(&channel, "42", false).is_none(),
            "ordinary room state must not bypass the failed provider cooldown"
        );
        let now = std::time::Instant::now();
        assert!(registry
            .take_channel_emote_retries(now, MAX_CHANNEL_LOADS)
            .is_empty());
        let retries =
            registry.take_channel_emote_retries(now + Duration::from_secs(60), MAX_CHANNEL_LOADS);
        assert_eq!(retries.len(), 1);
        assert_eq!(retries[0].channel, channel);
        assert_eq!(retries[0].room_id, "42");
        assert_eq!(retries[0].providers, ["7tv"]);
        assert!(retries[0].generation > generation);
    }

    #[tokio::test]
    async fn early_provider_failure_cannot_restart_a_pending_channel_load() {
        let index = index();
        let channel = ChannelId::new("room");
        let generation = {
            let mut registry = index.write().unwrap();
            registry.open_channel(&channel);
            registry.begin_channel_load(&channel, "42", false).unwrap()
        };
        let (release, pending) = tokio::sync::oneshot::channel();
        let providers: FuturesUnordered<ProviderLoad<'static>> = [
            async { ("bttv", None) }.boxed(),
            async move {
                pending.await.unwrap();
                ("7tv", Some(vec![emote("7tv", "Available")]))
            }
            .boxed(),
        ]
        .into_iter()
        .collect();
        let (events, mut received) = mpsc::channel(4);
        let mut publish = Box::pin(publish_channel_loads(
            &channel, generation, providers, &index, &events,
        ));
        assert!(matches!(futures_util::poll!(&mut publish), Poll::Pending));
        assert!(index
            .write()
            .unwrap()
            .begin_channel_load(&channel, "42", false)
            .is_none());
        release.send(()).unwrap();
        publish.await;
        assert_eq!(codes(receive(&mut received).await), ["Available"]);
        assert!(matches!(receive(&mut received).await,
            AppEvent::ChannelEmotesLoaded { channel: loaded, count: 1 } if loaded == channel));
        let mut registry = index.write().unwrap();
        assert!(registry.begin_channel_load(&channel, "42", false).is_none());
        let manual_refresh = registry.begin_channel_load(&channel, "42", true).unwrap();
        assert!(manual_refresh > generation);
        assert!(registry.is_current_channel_load(&channel, manual_refresh));
        assert!(!registry.is_current_channel_load(&channel, generation));
    }

    #[tokio::test]
    async fn channel_loads_share_a_budget_coalesce_refreshes_and_skip_obsolete_queued_work() {
        use std::collections::HashSet;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let index = index();
        let limit = Arc::new(tokio::sync::Semaphore::new(MAX_CHANNEL_LOADS));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (started, mut starts) = mpsc::unbounded_channel();
        let (events, _received) = mpsc::channel(64);
        let mut tasks = Vec::new();
        let mut releases = Vec::new();
        let mut channels = Vec::new();
        for n in 0..MAX_CHANNEL_LOADS + 2 {
            let channel = if n % 2 == 0 {
                ChannelId::new(&format!("room{n}"))
            } else {
                ChannelId::kick(&format!("room{n}"))
            };
            let generation = {
                let mut registry = index.write().unwrap();
                registry.open_channel(&channel);
                registry.begin_channel_load(&channel, "42", false).unwrap()
            };
            let (release, pending) = tokio::sync::oneshot::channel();
            releases.push(Some(release));
            channels.push(channel.clone());
            let started = started.clone();
            let active = active.clone();
            let peak = peak.clone();
            let providers: FuturesUnordered<ProviderLoad<'static>> = [async move {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                started.send(n).unwrap();
                pending.await.unwrap();
                active.fetch_sub(1, Ordering::SeqCst);
                ("ffz", None)
            }
            .boxed()]
            .into_iter()
            .collect();
            let task_index = index.clone();
            let events = events.clone();
            let limit = limit.clone();
            tasks.push(Some(tokio::spawn(async move {
                publish_channel_loads_with_limit(
                    &channel,
                    generation,
                    providers,
                    &task_index,
                    &events,
                    &limit,
                )
                .await;
            })));
        }

        let mut running = HashSet::new();
        for _ in 0..MAX_CHANNEL_LOADS {
            running.insert(
                tokio::time::timeout(Duration::from_secs(3), starts.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        tokio::task::yield_now().await;
        assert!(starts.try_recv().is_err());
        assert_eq!(active.load(Ordering::SeqCst), MAX_CHANNEL_LOADS);
        for channel in &channels {
            for _ in 0..64 {
                assert!(index
                    .write()
                    .unwrap()
                    .begin_channel_load(channel, "42", true)
                    .is_none());
            }
        }

        let queued: Vec<_> = (0..channels.len())
            .filter(|n| !running.contains(n))
            .collect();
        assert_eq!(queued.len(), 2);
        index.write().unwrap().close_channel(&channels[queued[0]]);
        index
            .write()
            .unwrap()
            .reconnect_channel(&channels[queued[1]]);
        let first = *running.iter().next().unwrap();
        releases[first].take().unwrap().send(()).unwrap();
        for n in &queued {
            tokio::time::timeout(Duration::from_secs(3), tasks[*n].take().unwrap())
                .await
                .unwrap()
                .unwrap();
        }
        assert!(
            starts.try_recv().is_err(),
            "an obsolete queued provider started a request"
        );
        let expected_retry_channels: HashSet<_> =
            running.iter().map(|n| channels[*n].clone()).collect();
        for n in running {
            if let Some(release) = releases[n].take() {
                release.send(()).unwrap();
            }
        }
        for task in tasks.into_iter().flatten() {
            tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), MAX_CHANNEL_LOADS);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        let mut registry = index.write().unwrap();
        assert!(
            registry
                .begin_channel_load(&channels[first], "42", false)
                .is_none(),
            "ordinary room state must not bypass the failed provider cooldown"
        );
        let now = std::time::Instant::now();
        assert!(registry
            .take_channel_emote_retries(now, MAX_CHANNEL_LOADS)
            .is_empty());
        let retries =
            registry.take_channel_emote_retries(now + Duration::from_secs(60), MAX_CHANNEL_LOADS);
        assert_eq!(retries.len(), MAX_CHANNEL_LOADS);
        assert_eq!(
            retries
                .iter()
                .map(|retry| retry.channel.clone())
                .collect::<HashSet<_>>(),
            expected_retry_channels,
            "closed and replaced queued loads must not be retried"
        );
        assert!(retries
            .iter()
            .all(|retry| retry.room_id == "42" && retry.providers == ["ffz"]));
        assert!(registry
            .take_channel_emote_retries(now + Duration::from_secs(120), MAX_CHANNEL_LOADS)
            .is_empty());
    }

    #[tokio::test]
    async fn empty_channel_provider_replaces_the_old_set_without_image_work() {
        let index = index();
        let channel = ChannelId::new("room");
        let generation = {
            let mut registry = index.write().unwrap();
            registry.open_channel(&channel);
            let generation = registry.begin_channel_load(&channel, "42", false).unwrap();
            registry.replace_channel(&channel, generation, "7tv", Some(vec![emote("7tv", "Old")]));
            generation
        };
        let providers: FuturesUnordered<ProviderLoad<'static>> =
            [async { ("7tv", Some(Vec::new())) }.boxed()]
                .into_iter()
                .collect();
        let (events, mut received) = mpsc::channel(4);
        publish_channel_loads(&channel, generation, providers, &index, &events).await;
        assert!(codes(receive(&mut received).await).is_empty());
        assert!(matches!(
            receive(&mut received).await,
            AppEvent::ChannelEmotesLoaded { count: 0, .. }
        ));
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn failed_channel_retry_waits_for_completion_capacity_and_settles_after_recovery() {
        let index = index();
        let channel = ChannelId::new("room");
        let generation = {
            let mut registry = index.write().unwrap();
            registry.open_channel(&channel);
            registry.begin_channel_load(&channel, "42", false).unwrap()
        };
        let (events, mut received) = mpsc::channel(1);
        events
            .send(AppEvent::EmoteCatalogUpdated { emotes: Vec::new() })
            .await
            .unwrap();
        let limit = tokio::sync::Semaphore::new(1);
        let providers: FuturesUnordered<ProviderLoad<'static>> =
            [async { ("ffz", None) }.boxed()].into_iter().collect();
        let mut publish = Box::pin(publish_channel_loads_with_limit(
            &channel, generation, providers, &index, &events, &limit,
        ));
        assert!(matches!(futures_util::poll!(&mut publish), Poll::Pending));
        assert!(index
            .write()
            .unwrap()
            .take_channel_emote_retries(
                std::time::Instant::now() + Duration::from_secs(60),
                MAX_CHANNEL_LOADS
            )
            .is_empty());
        receive(&mut received).await;
        publish.await;
        assert!(matches!(
            receive(&mut received).await,
            AppEvent::ChannelEmotesLoaded { count: 0, .. }
        ));

        let retry = index
            .write()
            .unwrap()
            .take_channel_emote_retries(
                std::time::Instant::now() + Duration::from_secs(60),
                MAX_CHANNEL_LOADS,
            )
            .pop()
            .unwrap();
        assert_eq!(retry.providers, ["ffz"]);
        let recovered: FuturesUnordered<ProviderLoad<'static>> =
            [async { ("ffz", Some(vec![emote("ffz", "Recovered")])) }.boxed()]
                .into_iter()
                .collect();
        let publish = publish_channel_loads_with_limit(
            &channel,
            retry.generation,
            recovered,
            &index,
            &events,
            &limit,
        );
        tokio::join!(publish, async {
            assert_eq!(codes(receive(&mut received).await), ["Recovered"]);
            assert!(matches!(
                receive(&mut received).await,
                AppEvent::ChannelEmotesLoaded { count: 1, .. }
            ));
        });
        assert!(index
            .write()
            .unwrap()
            .take_channel_emote_retries(
                std::time::Instant::now() + Duration::from_secs(600),
                MAX_CHANNEL_LOADS
            )
            .is_empty());
    }

    #[tokio::test]
    async fn replaced_channel_load_cannot_publish_a_completion_after_backpressure() {
        let index = index();
        let channel = ChannelId::new("room");
        let generation = {
            let mut registry = index.write().unwrap();
            registry.open_channel(&channel);
            registry.begin_channel_load(&channel, "42", false).unwrap()
        };
        let (events, mut received) = mpsc::channel(1);
        events
            .send(AppEvent::EmoteCatalogUpdated { emotes: Vec::new() })
            .await
            .unwrap();
        let mut publish = Box::pin(publish_channel_loads(
            &channel,
            generation,
            FuturesUnordered::<ChannelProviderLoad<'static>>::new(),
            &index,
            &events,
        ));
        assert!(matches!(futures_util::poll!(&mut publish), Poll::Pending));
        index.write().unwrap().reconnect_channel(&channel);
        received.recv().await.unwrap();
        publish.await;
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn catalog_snapshot_is_captured_after_waiting_for_event_capacity() {
        let index = index();
        index
            .write()
            .unwrap()
            .replace_global("7tv", vec![emote("7tv", "Old")]);
        let (events, mut received) = mpsc::channel(1);
        events
            .send(AppEvent::EmoteCatalogUpdated { emotes: Vec::new() })
            .await
            .unwrap();
        let mut publish = Box::pin(send_emote_catalog(&index, &events));
        assert!(matches!(futures_util::poll!(&mut publish), Poll::Pending));
        index
            .write()
            .unwrap()
            .replace_global("7tv", vec![emote("7tv", "Current")]);
        received.recv().await.unwrap();
        publish.await;
        assert_eq!(codes(receive(&mut received).await), ["Current"]);
    }
}

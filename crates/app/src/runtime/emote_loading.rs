use crust_core::{events::AppEvent, model::ChannelId};
use crust_emotes::{
    providers::{
        BttvProvider, EmoteInfo, FfzProvider, KickProvider, SevenTvProvider, TwitchGlobalProvider,
    },
    EmoteProvider,
};
use futures_util::{future::BoxFuture, stream::FuturesUnordered, FutureExt, StreamExt};
use tokio::sync::mpsc;

use crate::seventv::{load_7tv_user_for_twitch, SevenTvCosmeticUpdate};

type ProviderLoad<'a> = BoxFuture<'a, (&'static str, Option<Vec<EmoteInfo>>)>;

/// Publish built-in globals immediately and load remote provider sets in the background.
pub(crate) async fn load_global_emotes(index: &crate::EmoteIndex, evt_tx: &mpsc::Sender<AppEvent>) {
    let twitch = TwitchGlobalProvider::new().load_global().await;
    index.write().unwrap().replace_global("twitch", twitch);
    send_emote_catalog(index, evt_tx).await;

    let bttv = BttvProvider::new();
    let ffz = FfzProvider::new();
    let seven_tv = SevenTvProvider::new();
    let index = index.clone();
    let evt_tx = evt_tx.clone();
    tokio::spawn(async move {
        let providers: FuturesUnordered<ProviderLoad<'_>> = [
            async { ("bttv", Some(bttv.load_global().await)) }.boxed(),
            async { ("ffz", Some(ffz.load_global().await)) }.boxed(),
            async { ("7tv", Some(seven_tv.load_global().await)) }.boxed(),
        ]
        .into_iter()
        .collect();
        publish_global_loads(providers, &index, &evt_tx).await;
    });
}

async fn publish_global_loads(
    mut providers: FuturesUnordered<ProviderLoad<'_>>,
    index: &crate::EmoteIndex,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    while let Some((provider, emotes)) = providers.next().await {
        if evt_tx.is_closed() {
            return;
        }
        if let Some(emotes) = emotes {
            index.write().unwrap().replace_global(provider, emotes);
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
    let bttv = BttvProvider::new();
    let ffz = FfzProvider::new();
    let seven_tv = SevenTvProvider::new();
    let providers: FuturesUnordered<ProviderLoad<'_>> = [
        async { ("bttv", bttv.load_channel(room_id).await) }.boxed(),
        async { ("ffz", ffz.load_channel(room_id).await) }.boxed(),
        async { ("7tv", seven_tv.load_channel(room_id).await) }.boxed(),
    ]
    .into_iter()
    .collect();
    publish_channel_loads(channel, generation, providers, index, evt_tx).await;
}

pub(crate) async fn load_kick_channel_emotes(
    channel: &ChannelId,
    kick_user_id: u64,
    generation: u64,
    index: &crate::EmoteIndex,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let kick = KickProvider::new();
    let seven_tv = SevenTvProvider::new();
    let providers: FuturesUnordered<ProviderLoad<'_>> = [
        async { ("kick", kick.load_channel(channel.display_name()).await) }.boxed(),
        async {
            let emotes = if kick_user_id == 0 {
                Some(Vec::new())
            } else {
                seven_tv.load_kick_channel(&kick_user_id.to_string()).await
            };
            ("7tv", emotes)
        }
        .boxed(),
    ]
    .into_iter()
    .collect();
    publish_channel_loads(channel, generation, providers, index, evt_tx).await;
}

async fn publish_channel_loads(
    channel: &ChannelId,
    generation: u64,
    mut providers: FuturesUnordered<ProviderLoad<'_>>,
    index: &crate::EmoteIndex,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    // Each completed provider becomes available immediately. A slow response
    // cannot hold the other catalogs back, and images load when a widget uses them.
    while let Some((provider, emotes)) = providers.next().await {
        let succeeded = emotes.is_some();
        if evt_tx.is_closed()
            || !index
                .write()
                .unwrap()
                .replace_channel(channel, generation, provider, emotes)
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
            publish_global_loads(providers, &index, &events).await;
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
        assert!(
            index
                .write()
                .unwrap()
                .begin_channel_load(&channel, "42", false)
                .is_some(),
            "a failed provider must remain retryable"
        );
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
        assert!(index
            .write()
            .unwrap()
            .begin_channel_load(&channel, "42", false)
            .is_some());
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
            FuturesUnordered::new(),
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

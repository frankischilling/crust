use crust_core::{events::AppEvent, model::ChannelId};
use crust_emotes::{
    cache::EmoteCache,
    providers::{
        BttvProvider, EmoteInfo, FfzProvider, KickProvider, SevenTvProvider, TwitchGlobalProvider,
    },
    EmoteProvider,
};
use tokio::sync::mpsc;
use tracing::info;

use super::assets::fetch_emote_image;
use crate::seventv::{load_7tv_user_for_twitch, SevenTvCosmeticUpdate};

/// Publish built-in globals immediately and load remote provider sets in the background.
pub(crate) async fn load_global_emotes(
    index: &crate::EmoteIndex,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let twitch = TwitchGlobalProvider::new().load_global().await;
    let urls = twitch.iter().map(|emote| emote.url_1x.clone()).collect();
    index.write().unwrap().replace_global("twitch", twitch);
    send_emote_catalog(index, evt_tx).await;
    prefetch_emote_images(urls, cache, evt_tx);

    let bttv = BttvProvider::new();
    let ffz = FfzProvider::new();
    let seven_tv = SevenTvProvider::new();
    let index = index.clone();
    let cache = cache.clone();
    let evt_tx = evt_tx.clone();
    tokio::spawn(async move {
        let (b, f, s) = tokio::join!(
            bttv.load_global(),
            ffz.load_global(),
            seven_tv.load_global()
        );
        let urls = b
            .iter()
            .chain(&f)
            .chain(&s)
            .map(|emote| emote.url_1x.clone())
            .collect();
        {
            let mut registry = index.write().unwrap();
            registry.replace_global("bttv", b);
            registry.replace_global("ffz", f);
            registry.replace_global("7tv", s);
        }
        send_emote_catalog(&index, &evt_tx).await;
        prefetch_emote_images(urls, &cache, &evt_tx);
    });
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
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let bttv = BttvProvider::new();
    let ffz = FfzProvider::new();
    let seven_tv = SevenTvProvider::new();
    let (b, f, s) = tokio::join!(
        bttv.load_channel(room_id),
        ffz.load_channel(room_id),
        seven_tv.load_channel(room_id),
    );
    let emotes = vec![("bttv", b), ("ffz", f), ("7tv", s)];
    complete_channel_load(channel, generation, emotes, index, cache, evt_tx).await;
}

pub(crate) async fn load_kick_channel_emotes(
    channel: &ChannelId,
    kick_user_id: u64,
    generation: u64,
    index: &crate::EmoteIndex,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let kick = KickProvider::new();
    let seven_tv = SevenTvProvider::new();
    let (native, third_party) = tokio::join!(kick.load_channel(channel.display_name()), async {
        if kick_user_id == 0 {
            Some(Vec::new())
        } else {
            seven_tv.load_kick_channel(&kick_user_id.to_string()).await
        }
    });
    let emotes = vec![("kick", native), ("7tv", third_party)];
    complete_channel_load(channel, generation, emotes, index, cache, evt_tx).await;
}

async fn complete_channel_load(
    channel: &ChannelId,
    generation: u64,
    providers: Vec<(&str, Option<Vec<EmoteInfo>>)>,
    index: &crate::EmoteIndex,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    let urls = providers
        .iter()
        .filter_map(|(_, emotes)| emotes.as_ref())
        .flatten()
        .map(|emote| emote.url_1x.clone())
        .collect();
    let count = {
        let mut registry = index.write().unwrap();
        for (provider, emotes) in providers {
            if !registry.replace_channel(channel, generation, provider, emotes) {
                return;
            }
        }
        registry.channel_emote_count(channel)
    };
    send_emote_catalog(index, evt_tx).await;
    let _ = evt_tx
        .send(AppEvent::ChannelEmotesLoaded {
            channel: channel.clone(),
            count,
        })
        .await;
    prefetch_emote_images(urls, cache, evt_tx);
}

/// Serialize publications so a slow sender cannot publish an obsolete snapshot last.
pub(crate) async fn send_emote_catalog(index: &crate::EmoteIndex, evt_tx: &mpsc::Sender<AppEvent>) {
    static PUBLISH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _publication = PUBLISH.lock().await;
    let entries = index.read().unwrap().catalog();
    let _ = evt_tx
        .send(AppEvent::EmoteCatalogUpdated { emotes: entries })
        .await;
}

pub(crate) async fn clear_personal_emotes(
    index: &crate::EmoteIndex,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    index.write().unwrap().set_account(None);
    send_emote_catalog(index, evt_tx).await;
}

/// Prefetch newly loaded images through the shared concurrency and retry limits.
pub(crate) fn prefetch_emote_images(
    urls: Vec<String>,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
) {
    if urls.is_empty() {
        return;
    }
    info!("Prefetching {} emote images...", urls.len());
    let _ = evt_tx.try_send(AppEvent::ImagePrefetchQueued { count: urls.len() });
    for url in urls {
        let cache = cache.clone();
        let evt_tx = evt_tx.clone();
        tokio::spawn(async move {
            fetch_emote_image(&url, &cache, &evt_tx).await;
        });
    }
}

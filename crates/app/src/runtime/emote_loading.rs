use std::sync::Arc;

use crust_core::{
    events::AppEvent,
    model::{ChannelId, EmoteCatalogEntry},
};
use crust_emotes::{
    cache::EmoteCache,
    providers::{
        BttvProvider, EmoteInfo, FfzProvider, KickProvider, SevenTvProvider, TwitchGlobalProvider,
    },
    EmoteProvider,
};
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::assets::fetch_emote_image;

/// Load global emotes from BTTV, FFZ, 7TV and register in the shared index.
pub(crate) async fn load_global_emotes(
    index: &crate::EmoteIndex,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
    global_codes: &crate::GlobalCodes,
) {
    info!("Loading global emotes...");

    let twg = TwitchGlobalProvider::new();

    let t = twg.load_global().await;
    let total = t.len();
    info!("Loaded {total} global emotes (Twitch={})", t.len());

    let new_urls: Vec<String> = t.iter().map(|e| e.url_1x.clone()).collect();

    {
        let mut idx = index.write().unwrap();
        for e in t {
            idx.insert(crate::emote_key(&e.provider, &e.code), e);
        }
    }

    // Also register with EmoteCache if available
    if let Some(cache) = cache {
        let idx = index.read().unwrap();
        let emotes: Vec<EmoteInfo> = idx.values().cloned().collect();
        drop(idx);
        cache.register(emotes);
    }

    // Record global codes
    {
        let idx = index.read().unwrap();
        let mut gc = global_codes.write().unwrap();
        for info in idx.values() {
            gc.insert(info.code.clone());
        }
    }

    // Send catalog snapshot to the UI
    send_emote_catalog(index, evt_tx, global_codes).await;

    // Eagerly prefetch only the newly-loaded emote images
    prefetch_emote_images(new_urls, cache, evt_tx);
}

/// Load the authenticated viewer's personal 7TV emote set and merge into the global index.
pub(crate) async fn load_personal_7tv_emotes(
    user_id: &str,
    index: &crate::EmoteIndex,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
    global_codes: &crate::GlobalCodes,
) {
    info!("Loading personal 7TV emotes for user-id {user_id}");
    let stv = SevenTvProvider::new();
    let emotes = stv.load_channel(user_id).await;
    if emotes.is_empty() {
        info!("No personal 7TV emotes found for user-id {user_id}");
        return;
    }
    info!(
        "Loaded {} personal 7TV emotes for user-id {user_id}",
        emotes.len()
    );
    let new_urls: Vec<String> = emotes.iter().map(|e| e.url_1x.clone()).collect();
    {
        let mut idx = index.write().unwrap();
        for e in emotes {
            idx.insert(crate::emote_key(&e.provider, &e.code), e);
        }
    }
    send_emote_catalog(index, evt_tx, global_codes).await;
    prefetch_emote_images(new_urls, cache, evt_tx);
}

/// Load channel-specific emotes from BTTV, FFZ, 7TV.
pub(crate) async fn load_channel_emotes(
    channel_name: &str,
    room_id: &str,
    index: &crate::EmoteIndex,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
    global_codes: &crate::GlobalCodes,
) {
    info!("Loading channel emotes for #{channel_name} (room-id {room_id})");

    let bttv = BttvProvider::new();
    let ffz = FfzProvider::new();
    let stv = SevenTvProvider::new();

    let (b, f, s) = tokio::join!(
        bttv.load_channel(room_id),
        ffz.load_channel(room_id),
        stv.load_channel(room_id),
    );

    let total = b.len() + f.len() + s.len();
    if total == 0 {
        warn!("No channel emotes found for #{channel_name}");
        let _ = evt_tx
            .send(AppEvent::ChannelEmotesLoaded {
                channel: ChannelId::new(channel_name),
                count: 0,
            })
            .await;
        return;
    }
    info!(
        "Loaded {total} channel emotes for #{channel_name} (BTTV={}, FFZ={}, 7TV={})",
        b.len(),
        f.len(),
        s.len()
    );

    // Collect URLs of the newly-loaded emotes for prefetching.
    let new_urls: Vec<String> = f
        .iter()
        .chain(b.iter())
        .chain(s.iter())
        .map(|e| e.url_1x.clone())
        .collect();

    {
        let mut idx = index.write().unwrap();
        for e in f {
            idx.insert(crate::emote_key(&e.provider, &e.code), e);
        }
        for e in b {
            idx.insert(crate::emote_key(&e.provider, &e.code), e);
        }
        for e in s {
            idx.insert(crate::emote_key(&e.provider, &e.code), e);
        }
    }

    if let Some(cache) = cache {
        let idx = index.read().unwrap();
        let emotes: Vec<EmoteInfo> = idx.values().cloned().collect();
        drop(idx);
        cache.register(emotes);
    }

    // Send catalog snapshot to the UI
    send_emote_catalog(index, evt_tx, global_codes).await;

    let _ = evt_tx
        .send(AppEvent::ChannelEmotesLoaded {
            channel: ChannelId::new(channel_name),
            count: total,
        })
        .await;

    // Eagerly prefetch only the newly-loaded channel emote images
    prefetch_emote_images(new_urls, cache, evt_tx);
}

/// Load Kick channel emotes from Kick-native and 7TV(Kick) providers.
pub(crate) async fn load_kick_channel_emotes(
    channel: &ChannelId,
    kick_user_id: u64,
    index: &crate::EmoteIndex,
    cache: &Option<EmoteCache>,
    evt_tx: &mpsc::Sender<AppEvent>,
    global_codes: &crate::GlobalCodes,
) {
    let slug = channel.display_name().to_owned();
    info!(
        "Loading Kick emotes for {} (kick user-id {kick_user_id})",
        channel.display_name()
    );

    let kick = KickProvider::new();
    let stv = SevenTvProvider::new();

    let (kick_emotes, stv_emotes) = tokio::join!(kick.load_channel(&slug), async {
        if kick_user_id > 0 {
            stv.load_kick_channel(&kick_user_id.to_string()).await
        } else {
            vec![]
        }
    },);

    let total = kick_emotes.len() + stv_emotes.len();
    if total == 0 {
        warn!("No Kick emotes found for {}", channel.display_name());
        let _ = evt_tx
            .send(AppEvent::ChannelEmotesLoaded {
                channel: channel.clone(),
                count: 0,
            })
            .await;
        return;
    }

    info!(
        "Loaded {total} Kick channel emotes for {} (Kick={}, 7TV={})",
        channel.display_name(),
        kick_emotes.len(),
        stv_emotes.len(),
    );

    let new_urls: Vec<String> = kick_emotes
        .iter()
        .chain(stv_emotes.iter())
        .map(|e| e.url_1x.clone())
        .collect();

    {
        let mut idx = index.write().unwrap();
        for e in kick_emotes {
            idx.insert(crate::emote_key(&e.provider, &e.code), e);
        }
        for e in stv_emotes {
            idx.insert(crate::emote_key(&e.provider, &e.code), e);
        }
    }

    if let Some(cache) = cache {
        let idx = index.read().unwrap();
        let emotes: Vec<EmoteInfo> = idx.values().cloned().collect();
        drop(idx);
        cache.register(emotes);
    }

    send_emote_catalog(index, evt_tx, global_codes).await;

    let _ = evt_tx
        .send(AppEvent::ChannelEmotesLoaded {
            channel: channel.clone(),
            count: total,
        })
        .await;

    prefetch_emote_images(new_urls, cache, evt_tx);
}

/// Build a catalog snapshot from the emote index and send it to the UI.
pub(crate) async fn send_emote_catalog(
    index: &crate::EmoteIndex,
    evt_tx: &mpsc::Sender<AppEvent>,
    global_codes: &crate::GlobalCodes,
) {
    let entries: Vec<EmoteCatalogEntry> = {
        let idx = index.read().unwrap();
        let gc = global_codes.read().unwrap();
        idx.values()
            .map(|e| {
                let scope = if gc.contains(&e.code) {
                    "global"
                } else {
                    "channel"
                };
                EmoteCatalogEntry {
                    code: e.code.clone(),
                    provider: e.provider.clone(),
                    url: e.url_1x.clone(),
                    scope: scope.to_owned(),
                }
            })
            .collect()
    };
    let _ = evt_tx
        .send(AppEvent::EmoteCatalogUpdated { emotes: entries })
        .await;
}

/// Eagerly prefetch emote images in the background so they're available
/// in the emote picker and `:` autocomplete without waiting for lazy fetch.
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

    let sem = Arc::new(tokio::sync::Semaphore::new(20));

    for url in urls {
        let sem = sem.clone();
        let cache = cache.clone();
        let evt_tx = evt_tx.clone();
        tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            fetch_emote_image(&url, &cache, &evt_tx).await;
        });
    }
}

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crust_core::model::{
    Badge, Sender, SenderNamePaint, SenderNamePaintShadow, SenderNamePaintStop,
};
use crust_emotes::{
    providers::{personal_emote_set_id, read_bounded_json, SevenTvProvider},
    EmoteInfo,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::runtime::live_emotes::{LiveSource, Provider};

const SEVENTV_GQL_URL: &str = "https://api.7tv.app/v3/gql";
static NEXT_CATALOG_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SevenTvUserStyleRaw {
    pub(crate) color: Option<i32>,
    pub(crate) badge_id: Option<String>,
    pub(crate) paint_id: Option<String>,
    pub(crate) avatar_url: Option<String>,
}

pub(crate) struct SevenTvUserProfile {
    pub(crate) style: SevenTvUserStyleRaw,
    pub(crate) personal_emotes: Option<Vec<EmoteInfo>>,
    pub(crate) personal_source: Option<LiveSource>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SevenTvBadgeMeta {
    pub(crate) tooltip: Option<String>,
    pub(crate) url: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SevenTvPaintMeta {
    pub(crate) function: String,
    pub(crate) angle: Option<f32>,
    pub(crate) repeat: bool,
    pub(crate) image_url: Option<String>,
    pub(crate) shadows: Vec<SenderNamePaintShadow>,
    pub(crate) stops: Vec<SenderNamePaintStop>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SevenTvResolvedStyle {
    pub(crate) color_hex: Option<String>,
    pub(crate) badge: Option<Badge>,
    pub(crate) name_paint: Option<SenderNamePaint>,
    pub(crate) avatar_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SevenTvCosmeticKind {
    Paint,
    Badge,
}

#[derive(Debug, Clone)]
pub(crate) enum SevenTvLiveCosmeticUpdate {
    Connected,
    ConnectedSlot {
        slot: usize,
    },
    DisconnectedSlot {
        slot: usize,
    },
    /// Refresh cached users through the bounded profile queue when live coverage is unavailable.
    RefreshAll,
    CatalogRefreshStarted {
        generation: u64,
    },
    CatalogRefreshFinished {
        generation: u64,
        badges: HashMap<String, SevenTvBadgeMeta>,
        paints: HashMap<String, SevenTvPaintMeta>,
    },
    Paint {
        id: String,
        paint: SevenTvPaintMeta,
    },
    Badge {
        id: String,
        badge: SevenTvBadgeMeta,
    },
    Entitlement {
        kind: SevenTvCosmeticKind,
        cosmetic_id: String,
        twitch_user_ids: Vec<String>,
        granted: bool,
    },
    RefreshUsers {
        twitch_user_ids: Vec<String>,
    },
}

pub(crate) enum SevenTvCosmeticUpdate {
    UserStyle {
        twitch_user_id: String,
        generation: u64,
        profile: Option<SevenTvUserProfile>,
    },
    Live {
        epoch: u64,
        update: SevenTvLiveCosmeticUpdate,
    },
    /// Batch of Twitch user-ids discovered in history messages that need
    /// their 7TV styles resolved.
    BatchUserLookup {
        channel: crust_core::ChannelId,
        user_ids: Vec<String>,
    },
}

const MAX_CACHED_COSMETIC_USERS: usize = 2_048;
const MAX_COSMETIC_DEFINITIONS: usize = 16_384;

#[derive(Default)]
pub(crate) struct SevenTvCosmetics {
    badges: HashMap<String, SevenTvBadgeMeta>,
    paints: HashMap<String, SevenTvPaintMeta>,
    raw: HashMap<String, SevenTvUserStyleRaw>,
    resolved: HashMap<String, SevenTvResolvedStyle>,
    user_order: VecDeque<String>,
    refresh_users: HashSet<String>,
    refresh_order: VecDeque<String>,
    live_badges: HashSet<String>,
    live_paints: HashSet<String>,
    live_connections: HashMap<usize, (u64, bool)>,
    catalog_generation: u64,
    pending_catalog: Option<u64>,
    catalog_live_badges: HashSet<String>,
    catalog_live_paints: HashSet<String>,
}

#[derive(Default)]
pub(crate) struct SevenTvCosmeticEffects {
    pub(crate) changed: Vec<(String, SevenTvResolvedStyle)>,
    pub(crate) invalidate: Vec<String>,
    pub(crate) refresh: Vec<String>,
}

impl SevenTvCosmetics {
    pub(crate) fn resolved(&self, user_id: &str) -> Option<&SevenTvResolvedStyle> {
        self.resolved.get(user_id)
    }

    pub(crate) fn forget(&mut self, user_id: &str) {
        self.raw.remove(user_id);
        self.resolved.remove(user_id);
        self.user_order.retain(|user| user != user_id);
        self.complete_refresh(user_id);
    }

    fn complete_refresh(&mut self, user_id: &str) {
        if self.refresh_users.remove(user_id) {
            self.refresh_order.retain(|user| user != user_id);
        }
    }

    /// Recover a canceled full profile without moving an already queued reconnect refresh.
    pub(crate) fn retry_profile_lookup(&mut self, user_id: &str) {
        if !self.refresh_users.contains(user_id) {
            self.queue_refresh(user_id);
        }
    }

    fn queue_refresh(&mut self, user_id: &str) {
        if !valid_cosmetic_id(user_id) {
            return;
        }
        if self.refresh_users.contains(user_id) {
            self.refresh_order.retain(|user| user != user_id);
        } else {
            while self.refresh_order.len() >= MAX_CACHED_COSMETIC_USERS {
                if let Some(oldest) = self.refresh_order.pop_back() {
                    self.refresh_users.remove(&oldest);
                }
            }
            self.refresh_users.insert(user_id.to_owned());
        }
        self.refresh_order.push_front(user_id.to_owned());
    }

    /// A refresh remains pending until a current profile succeeds. Its next
    /// attempt can use another room when the previous request's room closes.
    pub(crate) fn refresh_candidates(&mut self, limit: usize) -> Vec<String> {
        let count = limit.min(self.refresh_order.len());
        let mut candidates = Vec::with_capacity(count);
        for _ in 0..count {
            if let Some(user_id) = self.refresh_order.pop_front() {
                candidates.push(user_id.clone());
                self.refresh_order.push_back(user_id);
            }
        }
        candidates
    }

    fn retain_user(&mut self, user_id: &str) {
        self.user_order.retain(|user| user != user_id);
        self.user_order.push_back(user_id.to_owned());
        while self.user_order.len() > MAX_CACHED_COSMETIC_USERS {
            if let Some(oldest) = self.user_order.pop_front() {
                self.raw.remove(&oldest);
                self.resolved.remove(&oldest);
                self.complete_refresh(&oldest);
            }
        }
    }

    fn resolve_user(&mut self, user_id: &str) -> Option<(String, SevenTvResolvedStyle)> {
        let raw = self.raw.get(user_id)?;
        let resolved = resolve_7tv_user_style(raw, &self.badges, &self.paints);
        if self.resolved.get(user_id) == Some(&resolved) {
            return None;
        }
        self.resolved.insert(user_id.to_owned(), resolved.clone());
        Some((user_id.to_owned(), resolved))
    }

    pub(crate) fn profile(
        &mut self,
        user_id: &str,
        style: SevenTvUserStyleRaw,
    ) -> Option<(String, SevenTvResolvedStyle)> {
        if !valid_cosmetic_id(user_id) {
            return None;
        }
        self.complete_refresh(user_id);
        self.retain_user(user_id);
        self.raw.insert(user_id.to_owned(), style);
        self.resolve_user(user_id)
    }

    pub(crate) fn catalog(
        &mut self,
        mut badges: HashMap<String, SevenTvBadgeMeta>,
        mut paints: HashMap<String, SevenTvPaintMeta>,
    ) -> Vec<(String, SevenTvResolvedStyle)> {
        // Socket objects may arrive while the startup catalog is in flight.
        for id in &self.live_badges {
            if let Some(badge) = self.badges.get(id) {
                badges.insert(id.clone(), badge.clone());
            }
        }
        for id in &self.live_paints {
            if let Some(paint) = self.paints.get(id) {
                paints.insert(id.clone(), paint.clone());
            }
        }
        self.badges = badges;
        self.paints = paints;
        let users: Vec<_> = self.raw.keys().cloned().collect();
        users
            .into_iter()
            .filter_map(|user| self.resolve_user(&user))
            .collect()
    }

    pub(crate) fn live(
        &mut self,
        epoch: u64,
        update: SevenTvLiveCosmeticUpdate,
    ) -> SevenTvCosmeticEffects {
        let mut effects = SevenTvCosmeticEffects::default();
        let connected = self
            .live_connections
            .values()
            .any(|&(current, active)| active && current == epoch);
        if !connected
            && !matches!(
                update,
                SevenTvLiveCosmeticUpdate::Connected
                    | SevenTvLiveCosmeticUpdate::ConnectedSlot { .. }
                    | SevenTvLiveCosmeticUpdate::DisconnectedSlot { .. }
                    | SevenTvLiveCosmeticUpdate::RefreshAll
                    | SevenTvLiveCosmeticUpdate::CatalogRefreshStarted { .. }
                    | SevenTvLiveCosmeticUpdate::CatalogRefreshFinished { .. }
            )
        {
            return effects;
        }
        match update {
            SevenTvLiveCosmeticUpdate::Connected => {
                return self.connect_slot(0, epoch);
            }
            SevenTvLiveCosmeticUpdate::ConnectedSlot { slot } => {
                return self.connect_slot(slot, epoch);
            }
            SevenTvLiveCosmeticUpdate::DisconnectedSlot { slot } => {
                if let Some((current, active)) = self.live_connections.get_mut(&slot) {
                    if *current == epoch {
                        *active = false;
                    }
                }
            }
            SevenTvLiveCosmeticUpdate::RefreshAll => {
                if epoch != 0 {
                    return effects;
                }
                effects.refresh = self.user_order.iter().rev().cloned().collect();
                effects.invalidate = effects.refresh.clone();
            }
            SevenTvLiveCosmeticUpdate::CatalogRefreshStarted { generation } => {
                if epoch == 0 && generation > self.catalog_generation {
                    self.catalog_generation = generation;
                    self.pending_catalog = Some(generation);
                    self.catalog_live_badges.clear();
                    self.catalog_live_paints.clear();
                }
            }
            SevenTvLiveCosmeticUpdate::CatalogRefreshFinished {
                generation,
                badges,
                paints,
            } => {
                if epoch != 0 || self.pending_catalog != Some(generation) {
                    return effects;
                }
                self.pending_catalog = None;
                self.live_badges = std::mem::take(&mut self.catalog_live_badges);
                self.live_paints = std::mem::take(&mut self.catalog_live_paints);
                info!(
                    "7TV catalog received: {} badges, {} paints",
                    badges.len(),
                    paints.len()
                );
                effects.changed = self.catalog(badges, paints);
            }
            SevenTvLiveCosmeticUpdate::Paint { id, paint } => {
                if self.paints.len() >= MAX_COSMETIC_DEFINITIONS && !self.paints.contains_key(&id) {
                    return effects;
                }
                self.live_paints.insert(id.clone());
                if self.pending_catalog.is_some() {
                    self.catalog_live_paints.insert(id.clone());
                }
                if self.paints.get(&id) == Some(&paint) {
                    return effects;
                }
                self.paints.insert(id.clone(), paint);
                let users: Vec<_> = self
                    .raw
                    .iter()
                    .filter(|(_, style)| style.paint_id.as_deref() == Some(id.as_str()))
                    .map(|(user, _)| user.clone())
                    .collect();
                effects.changed = users
                    .into_iter()
                    .filter_map(|user| self.resolve_user(&user))
                    .collect();
            }
            SevenTvLiveCosmeticUpdate::Badge { id, badge } => {
                if self.badges.len() >= MAX_COSMETIC_DEFINITIONS && !self.badges.contains_key(&id) {
                    return effects;
                }
                self.live_badges.insert(id.clone());
                if self.pending_catalog.is_some() {
                    self.catalog_live_badges.insert(id.clone());
                }
                if self.badges.get(&id) == Some(&badge) {
                    return effects;
                }
                self.badges.insert(id.clone(), badge);
                let users: Vec<_> = self
                    .raw
                    .iter()
                    .filter(|(_, style)| style.badge_id.as_deref() == Some(id.as_str()))
                    .map(|(user, _)| user.clone())
                    .collect();
                effects.changed = users
                    .into_iter()
                    .filter_map(|user| self.resolve_user(&user))
                    .collect();
            }
            SevenTvLiveCosmeticUpdate::Entitlement {
                kind,
                cosmetic_id,
                twitch_user_ids,
                granted,
            } => {
                for user_id in twitch_user_ids {
                    if !valid_cosmetic_id(&user_id) {
                        continue;
                    }
                    self.retain_user(&user_id);
                    let raw = self.raw.entry(user_id.clone()).or_default();
                    let selected = match kind {
                        SevenTvCosmeticKind::Paint => &mut raw.paint_id,
                        SevenTvCosmeticKind::Badge => &mut raw.badge_id,
                    };
                    if granted {
                        if selected.as_deref() == Some(cosmetic_id.as_str()) {
                            // The event is newer than an in-flight profile even
                            // when its selected value matches the cached value.
                            effects.invalidate.push(user_id.clone());
                            continue;
                        }
                        *selected = Some(cosmetic_id.clone());
                    } else if selected.as_deref() == Some(cosmetic_id.as_str()) {
                        *selected = None;
                    } else if selected.is_some() {
                        continue;
                    }
                    effects.invalidate.push(user_id.clone());
                    if let Some(changed) = self.resolve_user(&user_id) {
                        effects.changed.push(changed);
                    }
                }
            }
            SevenTvLiveCosmeticUpdate::RefreshUsers { twitch_user_ids } => {
                effects.refresh = twitch_user_ids;
                effects.invalidate = effects.refresh.clone();
            }
        }
        for user_id in effects.refresh.iter().rev() {
            self.queue_refresh(user_id);
        }
        effects
    }

    fn connect_slot(&mut self, slot: usize, epoch: u64) -> SevenTvCosmeticEffects {
        let mut effects = SevenTvCosmeticEffects::default();
        if epoch == 0 || slot >= 8 {
            return effects;
        }
        if let Some((current, _)) = self.live_connections.get(&slot) {
            if epoch <= *current {
                return effects;
            }
            effects.refresh = self.user_order.iter().rev().cloned().collect();
            effects.invalidate = effects.refresh.clone();
        }
        self.live_connections.insert(slot, (epoch, true));
        for user_id in effects.refresh.iter().rev() {
            self.queue_refresh(user_id);
        }
        effects
    }
}

#[derive(Debug, serde::Deserialize)]
struct SevenTvGraphQlResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<SevenTvGraphQlError>,
}

#[derive(Debug, serde::Deserialize)]
struct SevenTvGraphQlError {
    message: String,
}

impl<T> SevenTvGraphQlResponse<T> {
    fn successful_data(self) -> Option<T> {
        if !self.errors.is_empty() {
            let messages = self
                .errors
                .iter()
                .map(|error| error.message.as_str())
                .collect::<Vec<_>>()
                .join(" | ");
            warn!("7TV cosmetics GraphQL errors: {messages}");
            return None;
        }
        self.data
    }
}

#[derive(Debug, serde::Deserialize)]
struct SevenTvBadgeFile {
    name: String,
}

#[derive(serde::Deserialize)]
struct SevenTvBadgeNode {
    id: String,
    tooltip: Option<String>,
    host: SevenTvBadgeHost,
}

#[derive(serde::Deserialize)]
struct SevenTvBadgeHost {
    url: String,
    files: Vec<SevenTvBadgeFile>,
}

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(try_from = "i64")]
struct SevenTvPackedColor(i32);

impl TryFrom<i64> for SevenTvPackedColor {
    type Error = &'static str;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value < i64::from(i32::MIN) || value > i64::from(u32::MAX) {
            return Err("RGBA color exceeds 32 bits");
        }
        Ok(Self(value as u32 as i32))
    }
}

#[derive(serde::Deserialize)]
struct SevenTvPaintNode {
    id: String,
    #[serde(default)]
    function: Option<String>,
    #[serde(default)]
    angle: Option<f32>,
    #[serde(default)]
    repeat: bool,
    #[serde(default)]
    image_url: Option<String>,
    #[serde(default)]
    color: Option<SevenTvPackedColor>,
    #[serde(default)]
    stops: Vec<SevenTvPaintStopNode>,
    #[serde(default)]
    shadows: Vec<SevenTvPaintShadowNode>,
}

#[derive(serde::Deserialize)]
struct SevenTvPaintStopNode {
    at: f32,
    color: SevenTvPackedColor,
}

#[derive(serde::Deserialize)]
struct SevenTvPaintShadowNode {
    #[serde(default)]
    x_offset: f32,
    #[serde(default)]
    y_offset: f32,
    #[serde(default)]
    radius: f32,
    color: SevenTvPackedColor,
}

fn valid_cosmetic_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn parse_7tv_badge_node(badge: SevenTvBadgeNode) -> Option<(String, SevenTvBadgeMeta)> {
    if !valid_cosmetic_id(&badge.id) {
        return None;
    }
    let host = normalize_external_url(&badge.host.url)?;
    let file = choose_7tv_badge_file(&badge.host.files)?;
    Some((
        badge.id,
        SevenTvBadgeMeta {
            tooltip: badge.tooltip,
            url: seven_tv_badge_url(&host, &file.name),
        },
    ))
}

pub(crate) fn parse_7tv_live_cosmetic(
    kind: &str,
    value: serde_json::Value,
) -> Option<SevenTvLiveCosmeticUpdate> {
    match kind {
        "PAINT" => {
            let (id, paint) = parse_7tv_paint_node(serde_json::from_value(value).ok()?)?;
            Some(SevenTvLiveCosmeticUpdate::Paint { id, paint })
        }
        "BADGE" => {
            let (id, badge) = parse_7tv_badge_node(serde_json::from_value(value).ok()?)?;
            Some(SevenTvLiveCosmeticUpdate::Badge { id, badge })
        }
        _ => None,
    }
}

fn parse_7tv_paint_node(paint: SevenTvPaintNode) -> Option<(String, SevenTvPaintMeta)> {
    if !valid_cosmetic_id(&paint.id) {
        return None;
    }
    let function = paint
        .function
        .as_deref()
        .unwrap_or("linear-gradient")
        .trim()
        .to_ascii_lowercase()
        .replace('_', "-");
    if !matches!(
        function.as_str(),
        "linear-gradient" | "radial-gradient" | "url"
    ) {
        return None;
    }
    let stops = paint
        .stops
        .into_iter()
        .map(|stop| SenderNamePaintStop {
            at: stop.at,
            color: seven_tv_color_to_hex_with_alpha(stop.color.0),
        })
        .collect();
    let stops = normalize_7tv_paint_stops(
        stops,
        paint
            .color
            .map(|color| seven_tv_color_to_hex_with_alpha(color.0)),
    );
    let image_url = paint.image_url.as_deref().and_then(normalize_external_url);
    let shadows: Vec<_> = paint
        .shadows
        .into_iter()
        .filter(|shadow| {
            shadow.x_offset.is_finite()
                && shadow.y_offset.is_finite()
                && shadow.radius.is_finite()
                && shadow.radius > 0.0
        })
        .map(|shadow| SenderNamePaintShadow {
            x_offset: shadow.x_offset,
            y_offset: shadow.y_offset,
            radius: shadow.radius,
            color: seven_tv_color_to_hex_with_alpha(shadow.color.0),
        })
        .collect();
    let has_fill = if function == "url" {
        image_url.is_some()
    } else {
        !stops.is_empty()
    };
    if !has_fill && shadows.is_empty() {
        return None;
    }
    Some((
        paint.id,
        SevenTvPaintMeta {
            function,
            angle: paint
                .angle
                .filter(|angle| angle.is_finite())
                .map(|angle| angle.rem_euclid(360.0)),
            repeat: paint.repeat,
            image_url,
            shadows,
            stops,
        },
    ))
}

pub(crate) fn resolve_7tv_user_style(
    style: &SevenTvUserStyleRaw,
    badges: &HashMap<String, SevenTvBadgeMeta>,
    paints: &HashMap<String, SevenTvPaintMeta>,
) -> SevenTvResolvedStyle {
    let color_hex = style.color.and_then(seven_tv_color_to_hex);

    let badge = style
        .badge_id
        .as_ref()
        .and_then(|id| badges.get(id))
        .map(|b| Badge {
            name: "7tv".to_owned(),
            version: b.tooltip.clone().unwrap_or_else(|| "1".to_owned()),
            url: Some(b.url.clone()),
        });

    let name_paint = style
        .paint_id
        .as_ref()
        .and_then(|id| paints.get(id))
        .map(|paint| SenderNamePaint {
            function: paint.function.clone(),
            angle: paint.angle,
            repeat: paint.repeat,
            image_url: paint.image_url.clone(),
            shadows: paint.shadows.clone(),
            stops: paint.stops.clone(),
        });

    SevenTvResolvedStyle {
        color_hex,
        badge,
        name_paint,
        avatar_url: style.avatar_url.clone(),
    }
}

pub(crate) fn apply_7tv_cosmetics_to_sender(sender: &mut Sender, style: &SevenTvResolvedStyle) {
    if let Some(ref color) = style.color_hex {
        sender.color = Some(color.clone());
    }
    sender.name_paint = style.name_paint.clone();

    if let Some(ref badge) = style.badge {
        if let Some(existing) = sender
            .badges
            .iter_mut()
            .find(|b| b.name.eq_ignore_ascii_case("7tv"))
        {
            *existing = badge.clone();
        } else {
            sender.badges.insert(0, badge.clone());
        }
    } else {
        sender
            .badges
            .retain(|badge| !badge.name.eq_ignore_ascii_case("7tv"));
    }
}

/// Only a current profile response can replace a sender's cosmetics.
pub(crate) fn complete_7tv_user_profile(
    index: &crate::EmoteIndex,
    twitch_user_id: &str,
    generation: u64,
    profile: Option<SevenTvUserProfile>,
) -> Option<(
    crate::runtime::emote_registry::UserEmoteUpdate,
    Option<SevenTvUserStyleRaw>,
)> {
    let (style, emotes, source) = profile
        .map(|profile| {
            (
                Some(profile.style),
                profile.personal_emotes,
                profile.personal_source,
            )
        })
        .unwrap_or_default();
    let update = index.write().unwrap().complete_personal_load_with_source(
        twitch_user_id,
        generation,
        emotes,
        source,
    )?;
    Some((update, style))
}

pub(crate) fn next_cosmetic_catalog_generation() -> u64 {
    NEXT_CATALOG_GENERATION.fetch_add(1, Ordering::Relaxed)
}

pub(crate) async fn load_initial_7tv_cosmetics_catalog(
    client: reqwest::Client,
    updates: mpsc::Sender<SevenTvCosmeticUpdate>,
) {
    load_initial_7tv_cosmetics_catalog_at(&client, &updates, SEVENTV_GQL_URL).await;
}

async fn load_initial_7tv_cosmetics_catalog_at(
    client: &reqwest::Client,
    updates: &mpsc::Sender<SevenTvCosmeticUpdate>,
    endpoint: &str,
) {
    let mut retry_delay = Duration::from_secs(1);
    loop {
        let Ok(permit) = updates.reserve().await else {
            return;
        };
        let generation = next_cosmetic_catalog_generation();
        permit.send(SevenTvCosmeticUpdate::Live {
            epoch: 0,
            update: SevenTvLiveCosmeticUpdate::CatalogRefreshStarted { generation },
        });
        let result = tokio::select! {
            _ = updates.closed() => return,
            result = load_7tv_cosmetics_catalog_at(client, endpoint) => result,
        };
        if let Some((badges, paints)) = result {
            let _ = updates
                .send(SevenTvCosmeticUpdate::Live {
                    epoch: 0,
                    update: SevenTvLiveCosmeticUpdate::CatalogRefreshFinished {
                        generation,
                        badges,
                        paints,
                    },
                })
                .await;
            return;
        }
        // Only startup failures retry. A successful catalog ends this task;
        // lost live coverage has its own bounded refresh path.
        tokio::select! {
            _ = updates.closed() => return,
            _ = tokio::time::sleep(retry_delay) => {}
        }
        retry_delay = retry_delay.saturating_mul(2).min(Duration::from_secs(30));
    }
}

pub(crate) async fn load_7tv_cosmetics_catalog_at(
    client: &reqwest::Client,
    endpoint: &str,
) -> Option<(
    HashMap<String, SevenTvBadgeMeta>,
    HashMap<String, SevenTvPaintMeta>,
)> {
    #[derive(serde::Deserialize)]
    struct RespData {
        cosmetics: Cosmetics,
    }

    #[derive(serde::Deserialize)]
    struct Cosmetics {
        badges: Vec<SevenTvBadgeNode>,
        paints: Vec<SevenTvPaintNode>,
    }

    let query = r#"
        query {
            cosmetics {
                badges {
                    id
                    tooltip
                    host {
                        url
                        files {
                            name
                        }
                    }
                }
                paints {
                    id
                    function
                    angle
                    repeat
                    image_url
                    color
                    stops {
                        at
                        color
                    }
                    shadows {
                        x_offset
                        y_offset
                        radius
                        color
                    }
                }
            }
        }
    "#;

    let resp = match client
        .post(endpoint)
        .timeout(Duration::from_secs(10))
        .json(&serde_json::json!({ "query": query }))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
    {
        Ok(r) => r,
        Err(e) => {
            warn!("7TV cosmetics fetch failed: {e}");
            return None;
        }
    };

    let payload =
        match read_bounded_json::<SevenTvGraphQlResponse<RespData>>(resp, 8 * 1024 * 1024).await {
            Some(payload) => payload,
            None => {
                warn!("7TV cosmetics response was invalid or exceeded the size limit");
                return None;
            }
        };

    let Some(data) = payload.successful_data() else {
        return None;
    };

    let badges: HashMap<String, SevenTvBadgeMeta> = data
        .cosmetics
        .badges
        .into_iter()
        .filter_map(parse_7tv_badge_node)
        .collect();

    let paints: HashMap<String, SevenTvPaintMeta> = data
        .cosmetics
        .paints
        .into_iter()
        .filter_map(parse_7tv_paint_node)
        .collect();

    info!(
        "Loaded 7TV cosmetics catalog (badges={}, paints={})",
        badges.len(),
        paints.len()
    );

    Some((badges, paints))
}

pub(crate) async fn load_7tv_user_for_twitch(
    client: &reqwest::Client,
    twitch_user_id: &str,
) -> Option<SevenTvUserProfile> {
    if twitch_user_id.is_empty() {
        return None;
    }
    let query = r#"
        query($id: String!) {
            userByConnection(platform: TWITCH, id: $id) {
                id
                avatar_url
                style { color badge_id paint_id }
                emote_sets { id flags }
            }
        }
    "#;
    let response = match client
        .post(SEVENTV_GQL_URL)
        .json(&serde_json::json!({"query": query, "variables": {"id": twitch_user_id}}))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
    {
        Ok(response) => response,
        Err(error) => {
            debug!("7TV user lookup failed for {twitch_user_id}: {error}");
            return None;
        }
    };
    let response = read_bounded_json::<serde_json::Value>(response, 2 * 1024 * 1024).await?;
    if response
        .get("errors")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|errors| !errors.is_empty())
    {
        debug!("7TV user lookup returned GraphQL errors for {twitch_user_id}");
        return None;
    }
    let user = response.get("data")?.get("userByConnection")?;
    if user.is_null() {
        return Some(SevenTvUserProfile {
            style: SevenTvUserStyleRaw::default(),
            personal_emotes: Some(Vec::new()),
            personal_source: None,
        });
    }
    let style = parse_7tv_user_style(user)?;
    let personal_source = personal_source(twitch_user_id, user)?;
    let personal_emotes = SevenTvProvider::with_client(client.clone())
        .load_personal_from_user(user)
        .await;
    Some(SevenTvUserProfile {
        style,
        personal_emotes,
        personal_source: Some(personal_source),
    })
}

fn personal_source(twitch_user_id: &str, user: &serde_json::Value) -> Option<LiveSource> {
    let user_id = user.get("id")?.as_str()?;
    let sets = user.get("emote_sets")?.as_array()?;
    if user_id.is_empty()
        || user_id.len() > 128
        || sets.len() > 64
        || !user_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        || sets.iter().any(|set| {
            set.get("id").and_then(serde_json::Value::as_str).is_none()
                || set
                    .get("flags")
                    .and_then(serde_json::Value::as_u64)
                    .is_none()
        })
    {
        return None;
    }
    Some(LiveSource {
        provider: Provider::SevenTv,
        room_id: twitch_user_id.to_owned(),
        platform: "TWITCH".into(),
        user_id: Some(user_id.to_owned()),
        set_id: personal_emote_set_id(user).map(str::to_owned),
        connection_index: None,
    })
}

fn parse_7tv_user_style(response: &serde_json::Value) -> Option<SevenTvUserStyleRaw> {
    let user = response.get("user").unwrap_or(response).as_object()?;
    let style = match user.get("style")? {
        serde_json::Value::Object(style) => Some(style),
        serde_json::Value::Null => None,
        _ => return None,
    };
    let color = style
        .and_then(|style| style.get("color"))
        .and_then(serde_json::Value::as_i64)
        .and_then(|color| {
            SevenTvPackedColor::try_from(color)
                .ok()
                .map(|color| color.0)
        })
        .filter(|color| *color != 0);
    let badge_id = style
        .and_then(|style| {
            style
                .get("badge_id")
                .and_then(serde_json::Value::as_str)
                .or_else(|| style.get("badge")?.get("id")?.as_str())
        })
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    let paint = style.and_then(|style| {
        style
            .get("paint_id")
            .or_else(|| style.get("paint")?.get("id"))
    });
    if style.is_some() && paint.is_none() {
        return None;
    }
    let paint_id = match paint {
        Some(serde_json::Value::String(id)) => {
            (!id.trim().is_empty()).then(|| id.trim().to_owned())
        }
        Some(serde_json::Value::Null) | None => None,
        _ => return None,
    };
    let avatar_url = user
        .get("avatar_url")
        .and_then(serde_json::Value::as_str)
        .and_then(normalize_external_url);
    Some(SevenTvUserStyleRaw {
        color,
        badge_id,
        paint_id,
        avatar_url,
    })
}
fn seven_tv_color_to_rgba(color: i32) -> (u8, u8, u8, u8) {
    let raw = color as u32;
    let r = ((raw >> 24) & 0xFF) as u8;
    let g = ((raw >> 16) & 0xFF) as u8;
    let b = ((raw >> 8) & 0xFF) as u8;
    let a = (raw & 0xFF) as u8;
    (r, g, b, a)
}

fn seven_tv_color_to_hex(color: i32) -> Option<String> {
    if color == 0 {
        return None;
    }
    let (r, g, b, a) = seven_tv_color_to_rgba(color);
    // Plain username colors remain visible even when their alpha byte is unset.
    let a = if a == 0 { 255 } else { a };
    if a == 255 {
        Some(format!("#{r:02X}{g:02X}{b:02X}"))
    } else {
        Some(format!("#{r:02X}{g:02X}{b:02X}{a:02X}"))
    }
}

fn seven_tv_color_to_hex_with_alpha(color: i32) -> String {
    let (r, g, b, a) = seven_tv_color_to_rgba(color);
    if a == 255 {
        format!("#{r:02X}{g:02X}{b:02X}")
    } else {
        format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
    }
}

fn seven_tv_badge_url(host_url: &str, file_name: &str) -> String {
    let base = if host_url.starts_with("//") {
        format!("https:{host_url}")
    } else {
        host_url.to_owned()
    };
    format!("{}/{}", base.trim_end_matches('/'), file_name)
}

fn normalize_external_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("//") {
        return Some(format!("https:{trimmed}"));
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Some(trimmed.to_owned());
    }
    None
}

fn choose_7tv_badge_file(files: &[SevenTvBadgeFile]) -> Option<&SevenTvBadgeFile> {
    files
        .iter()
        .find(|f| f.name.starts_with("2x."))
        .or_else(|| files.iter().find(|f| f.name.starts_with("1x.")))
        .or_else(|| files.first())
}

fn normalize_7tv_paint_stops(
    mut stops: Vec<SenderNamePaintStop>,
    fallback_color: Option<String>,
) -> Vec<SenderNamePaintStop> {
    stops.retain(|s| !s.color.is_empty() && s.at.is_finite());

    if stops.is_empty() {
        if let Some(color) = fallback_color {
            return vec![
                SenderNamePaintStop {
                    at: 0.0,
                    color: color.clone(),
                },
                SenderNamePaintStop { at: 1.0, color },
            ];
        }
        return Vec::new();
    }

    // Preserve hard gradient edges by nudging duplicate or reversed
    // stop positions forward slightly.
    let mut last_stop = f32::NEG_INFINITY;
    for stop in &mut stops {
        if stop.at <= last_stop {
            stop.at = last_stop + 0.0000001;
        }
        last_stop = stop.at;
    }

    stops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_response_supplies_cosmetics_alongside_personal_set_metadata() {
        let profile = serde_json::json!({
            "style": {"color": 0x11223300u32, "badge_id": "supporter", "paint_id": "selected"},
            "avatar_url": "//cdn.7tv.app/avatar/example/1x.webp",
            "emote_sets": [{"id": "personal", "flags": 4}]
        });
        let style = parse_7tv_user_style(&profile).unwrap();
        assert_eq!(style.color, Some(0x11223300));
        assert_eq!(style.badge_id.as_deref(), Some("supporter"));
        assert_eq!(style.paint_id.as_deref(), Some("selected"));
        assert_eq!(
            style.avatar_url.as_deref(),
            Some("https://cdn.7tv.app/avatar/example/1x.webp")
        );
        let fallback = parse_7tv_user_style(&serde_json::json!({"user": {
            "style": {"color": 0, "badge": {"id": "fallback"}, "paint": {"id": "nested"}}
        }}))
        .unwrap();
        assert!(fallback.color.is_none());
        assert_eq!(fallback.badge_id.as_deref(), Some("fallback"));
        assert_eq!(fallback.paint_id.as_deref(), Some("nested"));
        assert!(parse_7tv_user_style(&serde_json::json!({"user": null})).is_none());
    }

    #[test]
    fn selected_paint_resolves_and_successful_removal_clears_it() {
        let paints = HashMap::from([(
            "selected".to_owned(),
            SevenTvPaintMeta {
                function: "url".to_owned(),
                angle: Some(45.0),
                repeat: true,
                image_url: Some("https://example.test/paint.webp".to_owned()),
                shadows: vec![SenderNamePaintShadow {
                    x_offset: 1.0,
                    y_offset: 2.0,
                    radius: 3.0,
                    color: "#112233FF".to_owned(),
                }],
                stops: vec![SenderNamePaintStop {
                    at: 0.0,
                    color: "#123456".to_owned(),
                }],
            },
        )]);
        let badges = HashMap::from([(
            "selected-badge".to_owned(),
            SevenTvBadgeMeta {
                tooltip: Some("Selected badge".to_owned()),
                url: "https://example.test/badge.webp".to_owned(),
            },
        )]);
        let selected = parse_7tv_user_style(&serde_json::json!({
            "style": {"paint_id": "selected", "badge_id": "selected-badge"}
        }))
        .unwrap();
        let mut sender = Sender {
            user_id: crust_core::UserId("42".to_owned()),
            login: "sender".to_owned(),
            display_name: "Sender".to_owned(),
            original_display_name: None,
            color: Some("#ABCDEF".to_owned()),
            name_paint: None,
            badges: vec![
                Badge {
                    name: "moderator".to_owned(),
                    version: "1".to_owned(),
                    url: None,
                },
                Badge {
                    name: "bttv".to_owned(),
                    version: "1".to_owned(),
                    url: None,
                },
            ],
        };
        apply_7tv_cosmetics_to_sender(
            &mut sender,
            &resolve_7tv_user_style(&selected, &badges, &paints),
        );
        let paint = sender.name_paint.as_ref().unwrap();
        assert_eq!(paint.function, "url");
        assert_eq!(
            paint.image_url.as_deref(),
            Some("https://example.test/paint.webp")
        );
        assert_eq!(paint.angle, Some(45.0));
        assert!(paint.repeat);
        assert_eq!(paint.shadows[0].radius, 3.0);
        assert_eq!(paint.stops[0].color, "#123456");
        assert_eq!(sender.badges[0].name, "7tv");
        let removed =
            parse_7tv_user_style(&serde_json::json!({"style": {"paint_id": null}})).unwrap();
        apply_7tv_cosmetics_to_sender(
            &mut sender,
            &resolve_7tv_user_style(&removed, &badges, &paints),
        );
        assert!(sender.name_paint.is_none());
        assert_eq!(sender.color.as_deref(), Some("#ABCDEF"));
        assert_eq!(
            sender
                .badges
                .iter()
                .map(|badge| badge.name.as_str())
                .collect::<Vec<_>>(),
            ["moderator", "bttv"]
        );
        assert!(parse_7tv_user_style(&serde_json::json!({"style": {"paint_id": 123}})).is_none());
        assert!(parse_7tv_user_style(&serde_json::json!({"style": {"badge_id": null}})).is_none());
        assert!(parse_7tv_user_style(&serde_json::json!({"style": "invalid"})).is_none());
    }

    #[test]
    fn stale_and_failed_profiles_cannot_replace_current_paint_metadata() {
        let index: crate::EmoteIndex = std::sync::Arc::new(std::sync::RwLock::new(
            crate::runtime::emote_registry::EmoteRegistry::default(),
        ));
        let (stale, current) = {
            let mut registry = index.write().unwrap();
            registry.set_account(Some("42"));
            let stale = registry.begin_personal_load("42", None).unwrap();
            registry.set_account(None);
            registry.set_account(Some("42"));
            (stale, registry.begin_personal_load("42", None).unwrap())
        };
        let profile = |paint: &str| {
            Some(SevenTvUserProfile {
                style: SevenTvUserStyleRaw {
                    paint_id: Some(paint.to_owned()),
                    ..Default::default()
                },
                personal_emotes: Some(Vec::new()),
                personal_source: None,
            })
        };
        assert!(complete_7tv_user_profile(&index, "42", stale, profile("old")).is_none());
        let (_, style) =
            complete_7tv_user_profile(&index, "42", current, profile("current")).unwrap();
        assert_eq!(style.unwrap().paint_id.as_deref(), Some("current"));
        assert!(complete_7tv_user_profile(&index, "42", stale, profile("old")).is_none());

        let retry = {
            let mut registry = index.write().unwrap();
            registry.set_account(Some("99"));
            registry.begin_personal_load("99", None).unwrap()
        };
        let (_, failed_style) = complete_7tv_user_profile(&index, "99", retry, None).unwrap();
        assert!(
            failed_style.is_none(),
            "failed requests must not emit a cosmetic replacement"
        );
    }

    #[test]
    fn partial_catalog_errors_do_not_replace_the_last_catalog() {
        let response: SevenTvGraphQlResponse<serde_json::Value> =
            serde_json::from_value(serde_json::json!({"data": {"cosmetics": {"paints": []}},
                "errors": [{"message": "paint lookup unavailable"}]}))
            .unwrap();
        assert!(response.successful_data().is_none());
        let empty: SevenTvGraphQlResponse<serde_json::Value> =
            serde_json::from_value(serde_json::json!({"data": {"cosmetics": {"paints": []}}}))
                .unwrap();
        assert!(empty.successful_data().is_some());
    }

    #[test]
    fn zero_is_absent_for_plain_names_and_transparent_for_paints() {
        assert_eq!(seven_tv_color_to_hex(0), None);
        assert_eq!(seven_tv_color_to_hex_with_alpha(0), "#00000000");
    }

    #[test]
    fn paint_alpha_is_preserved_while_plain_names_remain_visible() {
        let c = 0x11223300u32 as i32;
        assert_eq!(seven_tv_color_to_hex(c), Some("#112233".to_owned()));
        assert_eq!(seven_tv_color_to_hex_with_alpha(c), "#11223300");
        assert_eq!(seven_tv_color_to_hex_with_alpha(0x11223380), "#11223380");
        assert_eq!(seven_tv_color_to_hex_with_alpha(-1), "#FFFFFF");
    }

    #[test]
    fn paint_payloads_preserve_transparency_and_normalize_known_functions() {
        let payload = serde_json::json!({
            "id": "gradient", "function": "RADIAL_GRADIENT", "repeat": true, "angle": -45,
            "stops": [
                {"at": 0.0, "color": 0},
                {"at": 0.5, "color": 0xFF000080u32},
                {"at": 0.5, "color": -1}
            ],
            "shadows": [{"x_offset": 1, "y_offset": -2, "radius": 3, "color": 0x11223300}]
        });
        let SevenTvLiveCosmeticUpdate::Paint { paint, .. } =
            parse_7tv_live_cosmetic("PAINT", payload).unwrap()
        else {
            panic!("paint");
        };
        assert_eq!(paint.function, "radial-gradient");
        assert_eq!(paint.angle, Some(315.0));
        assert!(paint.repeat);
        assert_eq!(paint.stops[0].color, "#00000000");
        assert_eq!(paint.stops[1].color, "#FF000080");
        assert_eq!(paint.stops[2].color, "#FFFFFF");
        assert!(paint.stops[2].at > paint.stops[1].at);
        assert_eq!(paint.shadows[0].color, "#11223300");
    }

    #[test]
    fn paint_payloads_require_a_supported_function_and_usable_content() {
        for value in [
            serde_json::json!({"id": "bad", "function": "unknown", "color": -1}),
            serde_json::json!({"id": "bad", "function": "URL", "image_url": "javascript:bad", "color": -1}),
            serde_json::json!({"id": "bad", "function": "LINEAR_GRADIENT", "stops": []}),
            serde_json::json!({"id": "bad", "color": 0x1FFFFFFFFu64}),
        ] {
            assert!(parse_7tv_live_cosmetic("PAINT", value).is_none());
        }
        let SevenTvLiveCosmeticUpdate::Paint { paint, .. } = parse_7tv_live_cosmetic("PAINT",
            serde_json::json!({"id": "image", "function": "URL", "image_url": "//cdn.7tv.app/paint/example.webp"}))
            .unwrap() else { panic!("image paint"); };
        assert_eq!(
            paint.image_url.as_deref(),
            Some("https://cdn.7tv.app/paint/example.webp")
        );
        assert!(paint.stops.is_empty());
    }

    #[test]
    fn shadow_only_paints_keep_their_effect_when_fill_metadata_is_empty() {
        for function in ["LINEAR_GRADIENT", "URL"] {
            let SevenTvLiveCosmeticUpdate::Paint { paint, .. } = parse_7tv_live_cosmetic("PAINT",
                serde_json::json!({"id": "shadow", "function": function, "stops": [], "image_url": null,
                    "color": null, "shadows": [{"x_offset": 1, "y_offset": 0, "radius": 2, "color": -1}]}))
                .unwrap() else { panic!("shadow paint"); };
            assert!(paint.stops.is_empty());
            assert!(paint.image_url.is_none());
            assert_eq!(paint.shadows.len(), 1);
        }
        assert!(parse_7tv_live_cosmetic(
            "PAINT",
            serde_json::json!({"id": "invalid", "function": "LINEAR_GRADIENT",
            "shadows": [{"radius": -1, "color": -1}]})
        )
        .is_none());
    }

    #[test]
    #[ignore = "requires a public catalog capture in CRUST_7TV_CATALOG_PATH"]
    fn captured_public_catalog_accepts_supported_paint_definitions() {
        use serde_json::Value;
        use std::collections::BTreeMap;

        fn packed_color(value: &Value) -> bool {
            value
                .as_i64()
                .is_some_and(|color| (i64::from(i32::MIN)..=i64::from(u32::MAX)).contains(&color))
        }

        fn finite_number(value: &Value) -> bool {
            value
                .as_f64()
                .is_some_and(|number| (number as f32).is_finite())
        }

        fn optional(value: Option<&Value>, valid: fn(&Value) -> bool) -> bool {
            value.is_none_or(|value| value.is_null() || valid(value))
        }

        fn array_field<'a>(value: &'a Value, field: &str) -> Option<&'a [Value]> {
            match value.get(field) {
                None => Some(&[]),
                Some(Value::Array(values)) => Some(values),
                _ => None,
            }
        }

        let path = std::env::var_os("CRUST_7TV_CATALOG_PATH")
            .expect("set CRUST_7TV_CATALOG_PATH to a captured cosmetics GraphQL response");
        let bytes = std::fs::read(path).expect("read captured public cosmetics catalog");
        let payload: Value = serde_json::from_slice(&bytes).expect("catalog must be valid JSON");
        let paints = payload
            .pointer("/data/cosmetics/paints")
            .and_then(Value::as_array)
            .expect("catalog must contain data.cosmetics.paints");
        assert!(!paints.is_empty(), "catalog must contain paint definitions");

        let mut functions = BTreeMap::<String, usize>::new();
        let mut accepted_functions = BTreeMap::<String, usize>::new();
        let (mut accepted, mut expected, mut shadow_only) = (0, 0, 0);
        let (mut invalid, mut unsupported, mut empty, mut decode_errors) = (0, 0, 0, 0);
        let mut rejected_valid = Vec::new();
        for (index, raw) in paints.iter().enumerate() {
            let function = raw
                .get("function")
                .and_then(Value::as_str)
                .unwrap_or("linear-gradient")
                .trim()
                .to_ascii_lowercase()
                .replace('_', "-");
            *functions.entry(function.clone()).or_default() += 1;
            let supported = matches!(
                function.as_str(),
                "linear-gradient" | "radial-gradient" | "url"
            );
            let stops = array_field(raw, "stops");
            let shadows = array_field(raw, "shadows");
            // Check the public payload independently of the Rust DTO and decoder.
            let valid = raw["id"].as_str().is_some_and(valid_cosmetic_id)
                && optional(raw.get("function"), Value::is_string)
                && optional(raw.get("angle"), finite_number)
                && optional(raw.get("image_url"), Value::is_string)
                && optional(raw.get("color"), packed_color)
                && raw.get("repeat").is_none_or(Value::is_boolean)
                && stops.is_some_and(|stops| {
                    stops
                        .iter()
                        .all(|stop| finite_number(&stop["at"]) && packed_color(&stop["color"]))
                })
                && shadows.is_some_and(|shadows| {
                    shadows.iter().all(|shadow| {
                        packed_color(&shadow["color"])
                            && ["x_offset", "y_offset", "radius"]
                                .iter()
                                .all(|field| shadow.get(*field).is_none_or(finite_number))
                    })
                });
            let has_fill = if function == "url" {
                raw["image_url"].as_str().is_some_and(|url| {
                    ["//", "https://", "http://"]
                        .iter()
                        .any(|prefix| url.trim().starts_with(*prefix))
                })
            } else {
                stops.is_some_and(|stops| !stops.is_empty()) || packed_color(&raw["color"])
            };
            let has_shadow = shadows.is_some_and(|shadows| {
                shadows.iter().any(|shadow| {
                    shadow["radius"]
                        .as_f64()
                        .is_some_and(|radius| radius as f32 > 0.0)
                })
            });
            let should_accept = valid && supported && (has_fill || has_shadow);
            if !valid {
                invalid += 1;
            } else if !supported {
                unsupported += 1;
            } else if !should_accept {
                empty += 1;
            }
            expected += usize::from(should_accept);

            let decoded = serde_json::from_value::<SevenTvPaintNode>(raw.clone());
            decode_errors += usize::from(decoded.is_err());
            if let Some((_, paint)) = decoded.ok().and_then(parse_7tv_paint_node) {
                accepted += 1;
                *accepted_functions
                    .entry(paint.function.clone())
                    .or_default() += 1;
                let has_fill = if paint.function == "url" {
                    paint.image_url.is_some()
                } else {
                    !paint.stops.is_empty()
                };
                shadow_only += usize::from(!has_fill && !paint.shadows.is_empty());
            } else if should_accept {
                rejected_valid.push((index, raw["id"].as_str().unwrap_or_default()));
            }
        }

        eprintln!(
            "{}",
            serde_json::json!({
                "total": paints.len(), "accepted": accepted, "valid_supported": expected,
                "functions": functions, "accepted_functions": accepted_functions,
                "shadow_only": shadow_only, "invalid": invalid, "unsupported": unsupported,
                "empty": empty, "decode_errors": decode_errors,
            })
        );
        assert!(
            expected > 0,
            "catalog must contain usable supported definitions"
        );
        assert!(
            rejected_valid.is_empty(),
            "valid supported paints were rejected: {rejected_valid:?}"
        );
    }

    fn solid_paint(color: &str) -> SevenTvPaintMeta {
        SevenTvPaintMeta {
            function: "linear-gradient".to_owned(),
            stops: vec![SenderNamePaintStop {
                at: 0.0,
                color: color.to_owned(),
            }],
            ..Default::default()
        }
    }

    fn entitlement(id: &str, granted: bool) -> SevenTvLiveCosmeticUpdate {
        SevenTvLiveCosmeticUpdate::Entitlement {
            kind: SevenTvCosmeticKind::Paint,
            cosmetic_id: id.to_owned(),
            twitch_user_ids: vec!["42".to_owned()],
            granted,
        }
    }

    fn profile_with_paint(paint_id: Option<&str>) -> Option<SevenTvUserProfile> {
        Some(SevenTvUserProfile {
            style: SevenTvUserStyleRaw {
                paint_id: paint_id.map(str::to_owned),
                ..Default::default()
            },
            personal_emotes: Some(Vec::new()),
            personal_source: None,
        })
    }

    #[test]
    fn entitlement_retries_canceled_initial_profiles_for_grants_and_deletes() {
        for granted in [true, false] {
            let index = crate::EmoteIndex::default();
            let channel = crust_core::ChannelId::new("room");
            index.write().unwrap().open_channel(&channel);
            let initial = index
                .write()
                .unwrap()
                .begin_personal_load("42", Some(&channel))
                .unwrap();
            let mut cosmetics = SevenTvCosmetics::default();
            cosmetics.live(1, SevenTvLiveCosmeticUpdate::Connected);
            let effects = cosmetics.live(1, entitlement("selected", granted));
            for user_id in effects.invalidate {
                if index.write().unwrap().invalidate_personal_lookup(&user_id) {
                    cosmetics.retry_profile_lookup(&user_id);
                }
            }
            assert!(complete_7tv_user_profile(
                &index,
                "42",
                initial,
                profile_with_paint(Some("old"))
            )
            .is_none());
            assert_eq!(
                cosmetics.refresh_candidates(8),
                ["42"],
                "the first profile must not be lost when its sender stays quiet"
            );

            let replacement = index
                .write()
                .unwrap()
                .begin_personal_load("42", Some(&channel))
                .unwrap();
            let style = SevenTvUserStyleRaw {
                color: Some(0x112233FF),
                badge_id: Some("supporter".to_owned()),
                paint_id: granted.then(|| "selected".to_owned()),
                avatar_url: Some("https://example.test/avatar.webp".to_owned()),
            };
            let profile = SevenTvUserProfile {
                style: style.clone(),
                personal_emotes: Some(vec![EmoteInfo {
                    id: "personal".to_owned(),
                    code: "Personal".to_owned(),
                    provider: "7tv".to_owned(),
                    zero_width: false,
                    url_1x: "https://example.test/personal.webp".to_owned(),
                    url_2x: None,
                    url_4x: None,
                }]),
                personal_source: None,
            };
            let (update, fetched_style) =
                complete_7tv_user_profile(&index, "42", replacement, Some(profile)).unwrap();
            assert!(update.catalog_changed);
            cosmetics.profile("42", fetched_style.unwrap());
            assert_eq!(cosmetics.raw["42"], style);
            assert_eq!(
                index
                    .read()
                    .unwrap()
                    .resolve(&channel, "42", "Personal")
                    .unwrap()
                    .id,
                "personal"
            );
            assert!(cosmetics.refresh_candidates(8).is_empty());
        }
    }

    #[test]
    fn entitlement_updates_without_pending_profiles_do_not_queue_lookups() {
        let index = crate::EmoteIndex::default();
        let mut cosmetics = SevenTvCosmetics::default();
        cosmetics.live(1, SevenTvLiveCosmeticUpdate::Connected);
        cosmetics.profile("42", SevenTvUserStyleRaw::default());
        for granted in [true, true, false] {
            let effects = cosmetics.live(1, entitlement("selected", granted));
            for user_id in effects.invalidate {
                if index.write().unwrap().invalidate_personal_lookup(&user_id) {
                    cosmetics.retry_profile_lookup(&user_id);
                }
            }
            assert!(cosmetics.refresh_candidates(8).is_empty());
            assert_eq!(
                cosmetics.raw["42"].paint_id.as_deref(),
                granted.then_some("selected")
            );
        }
    }

    #[test]
    fn duplicate_live_grants_invalidate_older_profile_generations_without_repainting() {
        let index = crate::EmoteIndex::default();
        let channel = crust_core::ChannelId::new("room");
        index.write().unwrap().open_channel(&channel);
        let mut cosmetics = SevenTvCosmetics::default();
        cosmetics.live(1, SevenTvLiveCosmeticUpdate::Connected);
        cosmetics.profile(
            "42",
            SevenTvUserStyleRaw {
                paint_id: Some("selected".to_owned()),
                ..Default::default()
            },
        );
        let pending = index
            .write()
            .unwrap()
            .begin_personal_load("42", Some(&channel))
            .unwrap();
        let duplicate = cosmetics.live(1, entitlement("selected", true));
        assert!(duplicate.changed.is_empty());
        assert_eq!(duplicate.invalidate, ["42"]);
        for user_id in duplicate.invalidate {
            if index.write().unwrap().invalidate_personal_lookup(&user_id) {
                cosmetics.retry_profile_lookup(&user_id);
            }
        }
        assert!(
            complete_7tv_user_profile(&index, "42", pending, profile_with_paint(Some("old")))
                .is_none()
        );
        assert_eq!(cosmetics.raw["42"].paint_id.as_deref(), Some("selected"));
        assert_eq!(cosmetics.refresh_candidates(8), ["42"]);
    }

    #[test]
    fn reconnect_refresh_survives_a_closed_request_owner_and_ignores_its_late_result() {
        let index = crate::EmoteIndex::default();
        let first_room = crust_core::ChannelId::new("first");
        let second_room = crust_core::ChannelId::new("second");
        {
            let mut registry = index.write().unwrap();
            registry.open_channel(&first_room);
            registry.open_channel(&second_room);
        }
        let mut cosmetics = SevenTvCosmetics::default();
        cosmetics.live(1, SevenTvLiveCosmeticUpdate::Connected);
        cosmetics.profile(
            "42",
            SevenTvUserStyleRaw {
                paint_id: Some("selected".to_owned()),
                ..Default::default()
            },
        );
        let reconnect = cosmetics.live(2, SevenTvLiveCosmeticUpdate::Connected);
        for user_id in reconnect.invalidate {
            index.write().unwrap().invalidate_personal_lookup(&user_id);
        }
        assert_eq!(cosmetics.refresh_candidates(8), ["42"]);
        let old_request = index
            .write()
            .unwrap()
            .begin_personal_load("42", Some(&first_room))
            .unwrap();
        index.write().unwrap().close_channel(&first_room);
        assert!(complete_7tv_user_profile(
            &index,
            "42",
            old_request,
            profile_with_paint(Some("old"))
        )
        .is_none());
        assert_eq!(
            cosmetics.refresh_candidates(8),
            ["42"],
            "discarding a stale response cannot complete its refresh"
        );
        let current_request = index
            .write()
            .unwrap()
            .begin_personal_load("42", Some(&second_room))
            .unwrap();
        let (_, style) =
            complete_7tv_user_profile(&index, "42", current_request, profile_with_paint(None))
                .unwrap();
        cosmetics.profile("42", style.unwrap());
        assert!(cosmetics.refresh_candidates(8).is_empty());
        assert!(cosmetics.raw["42"].paint_id.is_none());
    }

    #[test]
    fn reconnect_refreshes_recent_users_first_and_keeps_the_retry_queue_bounded() {
        let mut cosmetics = SevenTvCosmetics::default();
        cosmetics.live(1, SevenTvLiveCosmeticUpdate::Connected);
        for user_id in ["oldest", "middle", "recent"] {
            cosmetics.profile(user_id, SevenTvUserStyleRaw::default());
        }
        let reconnect = cosmetics.live(2, SevenTvLiveCosmeticUpdate::Connected);
        assert_eq!(reconnect.refresh, ["recent", "middle", "oldest"]);
        cosmetics.retry_profile_lookup("oldest");
        cosmetics.retry_profile_lookup("recent");
        assert_eq!(cosmetics.refresh_candidates(2), ["recent", "middle"]);
        assert_eq!(cosmetics.refresh_candidates(2), ["oldest", "recent"]);
        cosmetics.profile("recent", SevenTvUserStyleRaw::default());
        assert_eq!(cosmetics.refresh_candidates(8), ["middle", "oldest"]);
        cosmetics.live(
            2,
            SevenTvLiveCosmeticUpdate::RefreshUsers {
                twitch_user_ids: (0..MAX_CACHED_COSMETIC_USERS + 5)
                    .map(|index| index.to_string())
                    .collect(),
            },
        );
        assert_eq!(cosmetics.refresh_users.len(), MAX_CACHED_COSMETIC_USERS);
        assert_eq!(cosmetics.refresh_order.len(), MAX_CACHED_COSMETIC_USERS);
        assert_eq!(cosmetics.refresh_candidates(2), ["0", "1"]);
        cosmetics.forget("0");
        assert!(!cosmetics.refresh_users.contains("0"));
        assert_eq!(cosmetics.refresh_order.len(), MAX_CACHED_COSMETIC_USERS - 1);
    }

    #[test]
    fn late_paint_objects_resolve_assignments_and_survive_older_catalogs() {
        let mut cosmetics = SevenTvCosmetics::default();
        cosmetics.live(1, SevenTvLiveCosmeticUpdate::Connected);
        let assigned = cosmetics.live(1, entitlement("selected", true));
        assert_eq!(assigned.invalidate, ["42"]);
        assert!(cosmetics.resolved("42").unwrap().name_paint.is_none());
        let arrived = cosmetics.live(
            1,
            SevenTvLiveCosmeticUpdate::Paint {
                id: "selected".to_owned(),
                paint: solid_paint("#123456"),
            },
        );
        assert_eq!(arrived.changed.len(), 1);
        assert_eq!(
            arrived.changed[0].1.name_paint.as_ref().unwrap().stops[0].color,
            "#123456"
        );
        assert!(cosmetics
            .catalog(
                HashMap::new(),
                HashMap::from([("selected".to_owned(), solid_paint("#FFFFFF")),])
            )
            .is_empty());
        assert_eq!(
            cosmetics
                .resolved("42")
                .unwrap()
                .name_paint
                .as_ref()
                .unwrap()
                .stops[0]
                .color,
            "#123456"
        );
    }

    #[test]
    fn entitlement_removal_only_clears_the_matching_selection() {
        let mut cosmetics = SevenTvCosmetics::default();
        cosmetics.live(1, SevenTvLiveCosmeticUpdate::Connected);
        cosmetics.catalog(
            HashMap::new(),
            HashMap::from([
                ("old".to_owned(), solid_paint("#FFFFFF")),
                ("current".to_owned(), solid_paint("#123456")),
            ]),
        );
        cosmetics.live(1, entitlement("old", true));
        cosmetics.live(1, entitlement("current", true));
        let stale = cosmetics.live(1, entitlement("old", false));
        assert!(stale.changed.is_empty());
        assert!(stale.invalidate.is_empty());
        assert_eq!(cosmetics.raw["42"].paint_id.as_deref(), Some("current"));
        let removed = cosmetics.live(1, entitlement("current", false));
        assert_eq!(removed.invalidate, ["42"]);
        assert!(removed.changed[0].1.name_paint.is_none());
    }

    #[test]
    fn reconnect_refreshes_cached_users_and_rejects_older_socket_updates() {
        let mut cosmetics = SevenTvCosmetics::default();
        cosmetics.live(1, SevenTvLiveCosmeticUpdate::Connected);
        cosmetics.live(1, entitlement("current", true));
        let reconnect = cosmetics.live(2, SevenTvLiveCosmeticUpdate::Connected);
        assert_eq!(reconnect.refresh, ["42"]);
        assert_eq!(reconnect.invalidate, ["42"]);
        assert!(cosmetics
            .live(1, entitlement("stale", true))
            .changed
            .is_empty());
        assert_eq!(cosmetics.raw["42"].paint_id.as_deref(), Some("current"));
        assert!(cosmetics
            .live(2, SevenTvLiveCosmeticUpdate::Connected)
            .refresh
            .is_empty());
    }

    #[test]
    fn live_user_cache_keeps_a_bounded_recent_history() {
        let mut cosmetics = SevenTvCosmetics::default();
        cosmetics.live(1, SevenTvLiveCosmeticUpdate::Connected);
        for index in 0..MAX_CACHED_COSMETIC_USERS + 5 {
            cosmetics.profile(&index.to_string(), SevenTvUserStyleRaw::default());
        }
        assert_eq!(cosmetics.raw.len(), MAX_CACHED_COSMETIC_USERS);
        assert_eq!(cosmetics.resolved.len(), MAX_CACHED_COSMETIC_USERS);
        assert_eq!(cosmetics.user_order.len(), MAX_CACHED_COSMETIC_USERS);
        assert!(cosmetics.resolved("0").is_none());
        cosmetics.forget("5");
        assert!(cosmetics.resolved("5").is_none());
        assert_eq!(cosmetics.user_order.len(), MAX_CACHED_COSMETIC_USERS - 1);
    }

    #[test]
    fn cosmetic_connection_epochs_remain_independent_and_retire_on_disconnect() {
        let mut cosmetics = SevenTvCosmetics::default();
        cosmetics.live(5, SevenTvLiveCosmeticUpdate::ConnectedSlot { slot: 1 });
        cosmetics.live(10, SevenTvLiveCosmeticUpdate::ConnectedSlot { slot: 0 });
        cosmetics.live(5, entitlement("first", true));
        assert_eq!(cosmetics.raw["42"].paint_id.as_deref(), Some("first"));
        cosmetics.live(10, entitlement("second", true));
        assert_eq!(cosmetics.raw["42"].paint_id.as_deref(), Some("second"));

        cosmetics.live(5, SevenTvLiveCosmeticUpdate::DisconnectedSlot { slot: 1 });
        assert!(cosmetics
            .live(5, entitlement("stale", true))
            .invalidate
            .is_empty());
        assert_eq!(cosmetics.raw["42"].paint_id.as_deref(), Some("second"));
        let reconnect = cosmetics.live(12, SevenTvLiveCosmeticUpdate::ConnectedSlot { slot: 1 });
        assert_eq!(reconnect.refresh, ["42"]);
        cosmetics.live(5, SevenTvLiveCosmeticUpdate::DisconnectedSlot { slot: 1 });
        cosmetics.live(12, entitlement("replacement", true));
        assert_eq!(cosmetics.raw["42"].paint_id.as_deref(), Some("replacement"));
        cosmetics.live(10, entitlement("still-connected", true));
        assert_eq!(
            cosmetics.raw["42"].paint_id.as_deref(),
            Some("still-connected")
        );
        cosmetics.live(5, SevenTvLiveCosmeticUpdate::ConnectedSlot { slot: 1 });
        assert!(cosmetics
            .live(5, entitlement("old-reconnect", true))
            .invalidate
            .is_empty());
    }

    #[test]
    fn unavailable_live_coverage_refreshes_recent_users_without_accepting_old_updates() {
        let mut cosmetics = SevenTvCosmetics::default();
        cosmetics.profile("42", SevenTvUserStyleRaw::default());
        cosmetics.profile("99", SevenTvUserStyleRaw::default());
        let refresh = cosmetics.live(0, SevenTvLiveCosmeticUpdate::RefreshAll);
        assert_eq!(refresh.refresh, ["99", "42"]);
        assert_eq!(refresh.invalidate, ["99", "42"]);
        assert_eq!(cosmetics.refresh_candidates(8), ["99", "42"]);
        assert!(cosmetics
            .live(0, entitlement("unowned", true))
            .invalidate
            .is_empty());
    }

    #[test]
    fn fresh_cosmetic_catalogs_preserve_only_edits_received_after_the_request_started() {
        let mut cosmetics = SevenTvCosmetics::default();
        cosmetics.live(10, SevenTvLiveCosmeticUpdate::ConnectedSlot { slot: 0 });
        cosmetics.live(
            10,
            SevenTvLiveCosmeticUpdate::Paint {
                id: "old".into(),
                paint: solid_paint("#111111"),
            },
        );
        cosmetics.profile(
            "42",
            SevenTvUserStyleRaw {
                paint_id: Some("old".into()),
                ..Default::default()
            },
        );
        cosmetics.profile(
            "99",
            SevenTvUserStyleRaw {
                paint_id: Some("during".into()),
                ..Default::default()
            },
        );
        cosmetics.live(
            0,
            SevenTvLiveCosmeticUpdate::CatalogRefreshStarted { generation: 100 },
        );
        cosmetics.live(
            10,
            SevenTvLiveCosmeticUpdate::Paint {
                id: "during".into(),
                paint: solid_paint("#222222"),
            },
        );
        let finished =
            |generation, old, during| SevenTvLiveCosmeticUpdate::CatalogRefreshFinished {
                generation,
                badges: HashMap::new(),
                paints: HashMap::from([
                    ("old".into(), solid_paint(old)),
                    ("during".into(), solid_paint(during)),
                ]),
            };
        cosmetics.live(0, finished(100, "#333333", "#444444"));
        assert_eq!(
            cosmetics
                .resolved("42")
                .unwrap()
                .name_paint
                .as_ref()
                .unwrap()
                .stops[0]
                .color,
            "#333333"
        );
        assert_eq!(
            cosmetics
                .resolved("99")
                .unwrap()
                .name_paint
                .as_ref()
                .unwrap()
                .stops[0]
                .color,
            "#222222"
        );
        cosmetics.live(
            10,
            SevenTvLiveCosmeticUpdate::Paint {
                id: "during".into(),
                paint: solid_paint("#555555"),
            },
        );
        cosmetics.live(0, finished(100, "#333333", "#666666"));
        assert_eq!(
            cosmetics
                .resolved("99")
                .unwrap()
                .name_paint
                .as_ref()
                .unwrap()
                .stops[0]
                .color,
            "#555555"
        );
        cosmetics.live(
            0,
            SevenTvLiveCosmeticUpdate::CatalogRefreshStarted { generation: 101 },
        );
        cosmetics.live(
            0,
            SevenTvLiveCosmeticUpdate::CatalogRefreshStarted { generation: 100 },
        );
        cosmetics.live(0, finished(100, "#333333", "#666666"));
        cosmetics.live(0, finished(101, "#333333", "#777777"));
        assert_eq!(
            cosmetics
                .resolved("99")
                .unwrap()
                .name_paint
                .as_ref()
                .unwrap()
                .stops[0]
                .color,
            "#777777"
        );
    }

    mod startup_catalog {
        use super::*;
        use std::future::Future;
        use std::time::Instant;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        async fn within<F: Future>(future: F) -> F::Output {
            tokio::time::timeout(Duration::from_secs(5), future)
                .await
                .expect("catalog fixture timed out")
        }

        struct Harness {
            http: TcpListener,
            updates: mpsc::Receiver<SevenTvCosmeticUpdate>,
            task: tokio::task::JoinHandle<()>,
        }

        impl Harness {
            async fn new() -> Self {
                let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let endpoint = format!("http://{}/gql", http.local_addr().unwrap());
                let client = reqwest::Client::builder().no_proxy().build().unwrap();
                let (sender, updates) = mpsc::channel(8);
                let task = tokio::spawn(async move {
                    load_initial_7tv_cosmetics_catalog_at(&client, &sender, &endpoint).await;
                });
                Self {
                    http,
                    updates,
                    task,
                }
            }

            async fn update(&mut self) -> SevenTvLiveCosmeticUpdate {
                match within(self.updates.recv()).await {
                    Some(SevenTvCosmeticUpdate::Live { epoch: 0, update }) => update,
                    _ => panic!("expected a fenced cosmetic catalog update"),
                }
            }

            async fn started(&mut self, cosmetics: &mut SevenTvCosmetics) -> u64 {
                let update = self.update().await;
                let SevenTvLiveCosmeticUpdate::CatalogRefreshStarted { generation } = update else {
                    panic!("expected the catalog request boundary");
                };
                cosmetics.live(0, update);
                generation
            }

            async fn request(&self) -> TcpStream {
                within(async {
                    let (mut stream, _) = self.http.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    let mut chunk = [0u8; 2048];
                    loop {
                        let count = stream.read(&mut chunk).await.unwrap();
                        assert!(count > 0, "catalog request ended before its body arrived");
                        bytes.extend_from_slice(&chunk[..count]);
                        assert!(bytes.len() <= 64 * 1024);
                        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().unwrap())
                                })
                                .unwrap_or(0);
                            if bytes.len() >= end + 4 + length {
                                return stream;
                            }
                        }
                    }
                })
                .await
            }
        }

        impl Drop for Harness {
            fn drop(&mut self) {
                self.task.abort();
            }
        }

        async fn respond(mut stream: TcpStream, status: u16, body: serde_json::Value) {
            let body = body.to_string();
            let response = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            within(stream.write_all(response.as_bytes())).await.unwrap();
            within(stream.shutdown()).await.unwrap();
        }

        fn catalog() -> serde_json::Value {
            serde_json::json!({"data": {"cosmetics": {
                "badges": [{"id": "supporter", "tooltip": "Supporter", "host": {
                    "url": "//cdn.7tv.app/badge/supporter",
                    "files": [{"name": "2x.webp"}]
                }}],
                "paints": [
                    {"id": "selected", "function": "LINEAR_GRADIENT", "color": 0x223344FFu32},
                    {"id": "during", "function": "LINEAR_GRADIENT", "color": 0x334455FFu32}
                ]
            }}})
        }

        #[tokio::test]
        async fn retries_a_failed_startup_catalog_and_preserves_live_edits() {
            let mut harness = Harness::new().await;
            let mut cosmetics = SevenTvCosmetics::default();
            cosmetics.profile(
                "42",
                SevenTvUserStyleRaw {
                    paint_id: Some("selected".into()),
                    badge_id: Some("supporter".into()),
                    ..Default::default()
                },
            );
            cosmetics.profile(
                "99",
                SevenTvUserStyleRaw {
                    paint_id: Some("during".into()),
                    ..Default::default()
                },
            );
            cosmetics.live(50, SevenTvLiveCosmeticUpdate::ConnectedSlot { slot: 0 });
            let first = harness.started(&mut cosmetics).await;
            let request = harness.request().await;
            let failed_at = Instant::now();
            respond(request, 503, serde_json::json!({})).await;
            let second = harness.started(&mut cosmetics).await;
            assert!(failed_at.elapsed() >= Duration::from_secs(1));
            assert!(second > first);
            let request = harness.request().await;
            cosmetics.live(
                50,
                SevenTvLiveCosmeticUpdate::Paint {
                    id: "during".into(),
                    paint: solid_paint("#ABCDEF"),
                },
            );
            respond(request, 200, catalog()).await;
            let changed = cosmetics.live(0, harness.update().await).changed;
            assert!(changed.iter().any(|(user, _)| user == "42"));
            let recovered = cosmetics.resolved("42").unwrap();
            assert_eq!(
                recovered.name_paint.as_ref().unwrap().stops[0].color,
                "#223344"
            );
            assert!(recovered.badge.is_some());
            assert_eq!(
                cosmetics
                    .resolved("99")
                    .unwrap()
                    .name_paint
                    .as_ref()
                    .unwrap()
                    .stops[0]
                    .color,
                "#ABCDEF"
            );
            within(&mut harness.task).await.unwrap();
            assert!(
                within(harness.updates.recv()).await.is_none(),
                "successful startup kept retrying"
            );
        }

        #[tokio::test]
        async fn a_late_startup_catalog_cannot_replace_a_newer_recovery_snapshot() {
            let mut harness = Harness::new().await;
            let mut cosmetics = SevenTvCosmetics::default();
            cosmetics.profile(
                "42",
                SevenTvUserStyleRaw {
                    paint_id: Some("selected".into()),
                    ..Default::default()
                },
            );
            let initial = harness.started(&mut cosmetics).await;
            let request = harness.request().await;
            let generation = next_cosmetic_catalog_generation();
            assert!(generation > initial);
            cosmetics.live(
                0,
                SevenTvLiveCosmeticUpdate::CatalogRefreshStarted { generation },
            );
            cosmetics.live(
                0,
                SevenTvLiveCosmeticUpdate::CatalogRefreshFinished {
                    generation,
                    badges: HashMap::new(),
                    paints: HashMap::from([("selected".into(), solid_paint("#00FF00"))]),
                },
            );
            respond(request, 200, catalog()).await;
            assert!(cosmetics.live(0, harness.update().await).changed.is_empty());
            assert_eq!(
                cosmetics
                    .resolved("42")
                    .unwrap()
                    .name_paint
                    .as_ref()
                    .unwrap()
                    .stops[0]
                    .color,
                "#00FF00"
            );
            within(&mut harness.task).await.unwrap();
        }

        #[tokio::test]
        async fn repeated_catalog_failures_wait_and_shutdown_cancels_the_retry() {
            let mut harness = Harness::new().await;
            let mut cosmetics = SevenTvCosmetics::default();
            harness.started(&mut cosmetics).await;
            let request = harness.request().await;
            let failed_at = Instant::now();
            respond(request, 503, serde_json::json!({})).await;
            harness.started(&mut cosmetics).await;
            assert!(failed_at.elapsed() >= Duration::from_secs(1));
            let request = harness.request().await;
            respond(
                request,
                200,
                serde_json::json!({
                    "errors": [{"message": "catalog temporarily unavailable"}]
                }),
            )
            .await;
            assert!(
                tokio::time::timeout(Duration::from_millis(150), harness.http.accept())
                    .await
                    .is_err(),
                "failed requests retried without backoff"
            );
            assert!(!harness.task.is_finished());
            harness.updates.close();
            within(&mut harness.task).await.unwrap();
        }

        #[tokio::test]
        async fn closing_the_receiver_cancels_an_in_flight_startup_request() {
            let mut harness = Harness::new().await;
            let mut cosmetics = SevenTvCosmetics::default();
            harness.started(&mut cosmetics).await;
            let _unanswered = harness.request().await;
            harness.updates.close();
            // The request deadline is ten seconds; shutdown must not wait for it.
            within(&mut harness.task).await.unwrap();
        }

        #[tokio::test]
        async fn a_stalled_catalog_retries_even_without_a_client_timeout() {
            let mut harness = Harness::new().await;
            let mut cosmetics = SevenTvCosmetics::default();
            let first = harness.started(&mut cosmetics).await;
            let _unanswered = harness.request().await;
            let Some(SevenTvCosmeticUpdate::Live {
                epoch: 0,
                update: SevenTvLiveCosmeticUpdate::CatalogRefreshStarted { generation },
            }) = tokio::time::timeout(Duration::from_secs(15), harness.updates.recv())
                .await
                .expect("the catalog request had no deadline")
            else {
                panic!("expected retry after a stalled response");
            };
            assert!(generation > first);
            harness.updates.close();
            within(&mut harness.task).await.unwrap();
        }
    }
}

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crust_core::model::{ChannelId, EmoteCatalogEntry, UserId};
use crust_emotes::EmoteInfo;

use super::live_emotes::reconcile::{ApplyResult, LiveState, SnapshotTicket};
use super::live_emotes::{EmoteDelta, LiveBinding, LiveScope, LiveSource, LiveTarget, Provider};

type ProviderEmotes = HashMap<String, HashMap<String, EmoteInfo>>;

const MAX_PENDING_USERS: usize = 64;
const MAX_CACHED_USERS: usize = 2_048;
const MAX_PERSONAL_EMOTES_PER_USER: usize = 256;
const MAX_PERSONAL_EMOTES: usize = 16_384;
const MAX_LIVE_PERSONAL_USERS: usize = 64;
const USER_LOOKUP_TTL: Duration = Duration::from_secs(10 * 60);
const CHANNEL_RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct UserLookup {
    generation: u64,
    channel: Option<ChannelId>,
    live_binding: Option<LiveBinding>,
}

pub(crate) struct UserEmoteUpdate {
    pub(crate) evicted_users: Vec<String>,
    pub(crate) catalog_changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TwitchFollowerTarget {
    pub(crate) channel: ChannelId,
    pub(crate) room_id: String,
    pub(crate) generation: u64,
}

pub(crate) struct ChannelEmoteRetry {
    pub(crate) channel: ChannelId,
    pub(crate) room_id: String,
    pub(crate) generation: u64,
    pub(crate) providers: Vec<String>,
}

#[derive(Clone, Default)]
struct ChannelEmotes {
    generation: u64,
    source_id: Option<String>,
    loading: bool,
    failed_providers: HashSet<String>,
    retry_at: Option<Instant>,
    emotes: ProviderEmotes,
    /// Definitions carried by incoming messages survive older fetched snapshots.
    inline_emotes: ProviderEmotes,
    live: HashMap<String, LiveState>,
    live_enabled: bool,
    live_generation: u64,
    live_room_id: Option<String>,
    twitch_followers: ProviderEmotes,
    twitch_follower_generation: Option<u64>,
}

#[derive(Clone)]
struct AuxiliaryLive {
    generation: u64,
    state: LiveState,
}

/// Emote definitions remain scoped to the channel or sender that owns them.
#[derive(Clone, Default)]
pub(crate) struct EmoteRegistry {
    global: ProviderEmotes,
    global_live: Option<AuxiliaryLive>,
    global_load_generation: u64,
    channels: HashMap<ChannelId, ChannelEmotes>,
    personal: HashMap<String, ProviderEmotes>,
    personal_live: HashMap<String, AuxiliaryLive>,
    whisper_senders: HashSet<String>,
    pending_users: HashMap<String, UserLookup>,
    deferred_users: HashMap<String, Option<ChannelId>>,
    deferred_user_order: VecDeque<String>,
    cached_users: HashMap<String, Instant>,
    cached_user_order: VecDeque<String>,
    personal_emote_count: usize,
    active_account: Option<String>,
    twitch_inventory: ProviderEmotes,
    twitch_inventory_generation: Option<u64>,
    next_generation: u64,
}

fn provider_maps(emotes: Vec<EmoteInfo>) -> ProviderEmotes {
    let mut providers = ProviderEmotes::new();
    for emote in emotes {
        providers
            .entry(emote.provider.clone())
            .or_default()
            .insert(emote.code.clone(), emote);
    }
    providers
}

fn lookup<'a>(emotes: &'a ProviderEmotes, provider: &str, code: &str) -> Option<&'a EmoteInfo> {
    emotes.get(provider)?.get(code)
}

fn trim_emotes(emotes: &mut HashMap<String, EmoteInfo>, limit: usize) -> bool {
    if emotes.len() <= limit {
        return false;
    }
    let mut codes: Vec<_> = emotes.keys().cloned().collect();
    codes.sort();
    for code in codes.into_iter().skip(limit) {
        emotes.remove(&code);
    }
    true
}

impl EmoteRegistry {
    pub(crate) fn begin_global_load(&mut self) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1);
        self.global_load_generation = self.next_generation;
        self.global_load_generation
    }

    pub(crate) fn is_current_global_load(&self, generation: u64) -> bool {
        self.global_load_generation == generation
    }

    pub(crate) fn replace_global_result(
        &mut self,
        generation: u64,
        provider: &str,
        emotes: Option<Vec<EmoteInfo>>,
    ) -> bool {
        if !self.is_current_global_load(generation) {
            return false;
        }
        let Some(emotes) = emotes else {
            return false;
        };
        let next = emotes
            .into_iter()
            .map(|emote| (emote.code.clone(), emote))
            .collect();
        if self.global.get(provider) == Some(&next) {
            return false;
        }
        self.global.insert(provider.to_owned(), next);
        true
    }

    #[cfg(test)]
    pub(crate) fn replace_global(&mut self, provider: &str, emotes: Vec<EmoteInfo>) {
        self.global.insert(
            provider.to_owned(),
            emotes
                .into_iter()
                .map(|emote| (emote.code.clone(), emote))
                .collect(),
        );
    }

    pub(crate) fn replace_global_live(
        &mut self,
        source: LiveSource,
        emotes: Vec<EmoteInfo>,
        expected: Option<LiveBinding>,
    ) -> bool {
        if source.provider != Provider::SevenTv
            || self.global_live.as_ref().map(|scope| scope.state.binding()) != expected
            || self
                .global_live
                .as_ref()
                .is_some_and(|scope| !scope.state.accepts_cached_source(&source))
        {
            return false;
        }
        match self.global_live.as_mut() {
            Some(scope) => scope.state.rebind(source),
            None => {
                self.next_generation = self.next_generation.wrapping_add(1);
                self.global_live = Some(AuxiliaryLive {
                    generation: self.next_generation,
                    state: LiveState::new(source),
                });
            }
        }
        let next = self
            .global_live
            .as_ref()
            .unwrap()
            .state
            .overlay_snapshot(emotes, self.global.get("7tv"));
        let changed = self.global.get("7tv") != Some(&next);
        self.global.insert("7tv".into(), next);
        changed
    }

    pub(crate) fn global_live_binding(&self) -> Option<LiveBinding> {
        self.global_live.as_ref().map(|scope| scope.state.binding())
    }

    pub(crate) fn open_channel(&mut self, channel: &ChannelId) {
        self.channels.entry(channel.clone()).or_default();
    }

    pub(crate) fn close_channel(&mut self, channel: &ChannelId) {
        self.channels.remove(channel);
        self.pending_users
            .retain(|_, lookup| lookup.channel.as_ref() != Some(channel));
        self.deferred_users
            .retain(|_, owner| owner.as_ref() != Some(channel));
        let deferred = &self.deferred_users;
        self.deferred_user_order
            .retain(|user| deferred.contains_key(user));
    }

    /// Keep visible emotes while allowing the next room state to refresh the set.
    pub(crate) fn reconnect_channel(&mut self, channel: &ChannelId) {
        self.next_generation = self.next_generation.wrapping_add(1);
        let scope = self.channels.entry(channel.clone()).or_default();
        scope.source_id = None;
        scope.generation = self.next_generation;
        scope.loading = false;
        scope.failed_providers.clear();
        scope.retry_at = None;
        scope.live_enabled = false;
        scope.live_generation = self.next_generation;
        scope.twitch_follower_generation = None;
    }

    /// Reserve a generation before spawning a load, coalescing active refreshes.
    pub(crate) fn begin_channel_load(
        &mut self,
        channel: &ChannelId,
        source_id: &str,
        force: bool,
    ) -> Option<u64> {
        let scope = self.channels.get_mut(channel)?;
        if scope.source_id.as_deref() == Some(source_id) && (scope.loading || !force) {
            return None;
        }
        self.next_generation = self.next_generation.wrapping_add(1);
        scope.generation = self.next_generation;
        scope.source_id = Some(source_id.to_owned());
        scope.loading = true;
        scope.failed_providers.clear();
        scope.retry_at = None;
        scope.live_enabled = true;
        if scope.live_room_id.as_deref() != Some(source_id) || scope.live_generation == 0 {
            scope.live_generation = self.next_generation;
            scope.live_room_id = Some(source_id.to_owned());
            scope.twitch_followers.clear();
            scope.twitch_follower_generation = None;
        }
        scope
            .live
            .retain(|_, state| state.source.room_id == source_id);
        Some(scope.generation)
    }

    pub(crate) fn is_current_channel_load(&self, channel: &ChannelId, generation: u64) -> bool {
        self.channels
            .get(channel)
            .is_some_and(|scope| scope.loading && scope.generation == generation)
    }

    /// Allow a failed provider to retry only after every provider has settled.
    pub(crate) fn complete_channel_load(&mut self, channel: &ChannelId, generation: u64) -> bool {
        self.complete_channel_load_at(channel, generation, Instant::now())
    }

    fn complete_channel_load_at(
        &mut self,
        channel: &ChannelId,
        generation: u64,
        now: Instant,
    ) -> bool {
        let Some(scope) = self.channels.get_mut(channel) else {
            return false;
        };
        if scope.generation != generation || !scope.loading {
            return false;
        }
        scope.loading = false;
        if !scope.failed_providers.is_empty() {
            scope.retry_at = Some(now + CHANNEL_RETRY_DELAY);
        }
        true
    }

    /// Reserve only failed providers after cooldown, within the channel load budget.
    pub(crate) fn take_channel_emote_retries(
        &mut self,
        now: Instant,
        limit: usize,
    ) -> Vec<ChannelEmoteRetry> {
        let available =
            limit.saturating_sub(self.channels.values().filter(|scope| scope.loading).count());
        if available == 0 {
            return Vec::new();
        }
        let mut due: Vec<_> = self
            .channels
            .iter()
            .filter(|(channel, scope)| {
                (channel.is_twitch() || channel.is_kick())
                    && scope.live_enabled
                    && !scope.loading
                    && !scope.failed_providers.is_empty()
            })
            .filter_map(|(channel, scope)| {
                let retry_at = scope.retry_at.filter(|at| *at <= now)?;
                let room_id = scope.live_room_id.as_ref()?.clone();
                let mut providers: Vec<_> = scope.failed_providers.iter().cloned().collect();
                providers.sort();
                Some((retry_at, channel.clone(), room_id, providers))
            })
            .collect();
        due.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.as_str().cmp(right.1.as_str()))
        });
        due.into_iter()
            .take(available)
            .filter_map(|(_, channel, room_id, providers)| {
                let generation = self.begin_channel_load(&channel, &room_id, true)?;
                Some(ChannelEmoteRetry {
                    channel,
                    room_id,
                    generation,
                    providers,
                })
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn replace_channel(
        &mut self,
        channel: &ChannelId,
        generation: u64,
        provider: &str,
        emotes: Option<Vec<EmoteInfo>>,
    ) -> bool {
        let expected_source = self.live_binding(channel, generation, provider);
        self.replace_channel_with_source(
            channel,
            generation,
            provider,
            emotes,
            None,
            expected_source,
        )
    }

    pub(crate) fn replace_channel_with_source(
        &mut self,
        channel: &ChannelId,
        generation: u64,
        provider: &str,
        emotes: Option<Vec<EmoteInfo>>,
        source: Option<LiveSource>,
        expected_source: Option<LiveBinding>,
    ) -> bool {
        let Some(scope) = self.channels.get_mut(channel) else {
            return false;
        };
        if scope.generation != generation || !scope.loading {
            return false;
        }
        if let Some(emotes) = emotes {
            // A set switch observed after this HTTP request began wins over it.
            if scope.live.get(provider).map(LiveState::binding) != expected_source {
                return true;
            }
            if let Some(source) = source.filter(|source| source.provider.name() == provider) {
                if scope
                    .live
                    .get(provider)
                    .is_some_and(|state| !state.accepts_cached_source(&source))
                {
                    return true;
                }
                match scope.live.get_mut(provider) {
                    Some(state) => state.rebind(source),
                    None => {
                        scope
                            .live
                            .insert(provider.to_owned(), LiveState::new(source));
                    }
                }
            }
            let next = match scope.live.get(provider) {
                Some(state) => state.overlay_snapshot(emotes, scope.emotes.get(provider)),
                None => emotes
                    .into_iter()
                    .map(|emote| (emote.code.clone(), emote))
                    .collect(),
            };
            scope.emotes.insert(provider.to_owned(), next);
        } else {
            // Keep the last usable set while other providers finish this generation.
            scope.failed_providers.insert(provider.to_owned());
        }
        true
    }

    pub(crate) fn live_binding(
        &self,
        channel: &ChannelId,
        generation: u64,
        provider: &str,
    ) -> Option<LiveBinding> {
        self.channels
            .get(channel)
            .filter(|scope| scope.generation == generation)?
            .live
            .get(provider)
            .map(LiveState::binding)
    }

    pub(crate) fn live_targets(&self) -> Vec<LiveTarget> {
        let mut targets = Vec::new();
        if let Some(scope) = &self.global_live {
            targets.push(LiveTarget {
                scope: LiveScope::Global,
                generation: scope.generation,
                binding_generation: scope.state.binding_generation,
                source: scope.state.source.clone(),
            });
        }
        if let Some(user) = &self.active_account {
            if let Some(scope) = self.personal_live.get(user) {
                targets.push(LiveTarget {
                    scope: LiveScope::Personal(user.clone()),
                    generation: scope.generation,
                    binding_generation: scope.state.binding_generation,
                    source: scope.state.source.clone(),
                });
            }
        }
        let mut channels: Vec<_> = self
            .channels
            .iter()
            .filter(|(_, scope)| scope.live_enabled)
            .flat_map(|(channel, scope)| {
                scope
                    .live
                    .values()
                    .filter(|state| {
                        state.source.provider.name() != "7tv"
                            || state.source.user_id.is_some()
                            || state.source.set_id.is_some()
                    })
                    .map(move |state| LiveTarget {
                        scope: LiveScope::Channel(channel.clone()),
                        generation: scope.live_generation,
                        binding_generation: state.binding_generation,
                        source: state.source.clone(),
                    })
            })
            .collect();
        channels.sort_by(|left, right| {
            let LiveScope::Channel(left) = &left.scope else {
                unreachable!()
            };
            let LiveScope::Channel(right) = &right.scope else {
                unreachable!()
            };
            left.as_str().cmp(right.as_str())
        });
        targets.extend(channels);
        let mut personal_count = usize::from(
            self.active_account
                .as_ref()
                .is_some_and(|user| self.personal_live.contains_key(user)),
        );
        for user in self
            .cached_user_order
            .iter()
            .rev()
            .filter(|user| Some(user.as_str()) != self.active_account.as_deref())
        {
            if personal_count >= MAX_LIVE_PERSONAL_USERS {
                break;
            }
            if let Some(scope) = self.personal_live.get(user) {
                targets.push(LiveTarget {
                    scope: LiveScope::Personal(user.clone()),
                    generation: scope.generation,
                    binding_generation: scope.state.binding_generation,
                    source: scope.state.source.clone(),
                });
                personal_count += 1;
            }
        }
        targets
    }

    fn live_state(&self, target: &LiveTarget) -> Option<&LiveState> {
        let (generation, state) = match &target.scope {
            LiveScope::Channel(channel) => {
                let scope = self
                    .channels
                    .get(channel)
                    .filter(|scope| scope.live_enabled)?;
                (
                    scope.live_generation,
                    scope.live.get(target.source.provider.name())?,
                )
            }
            LiveScope::Global => {
                let scope = self.global_live.as_ref()?;
                (scope.generation, &scope.state)
            }
            LiveScope::Personal(user) => {
                let scope = self.personal_live.get(user)?;
                (scope.generation, &scope.state)
            }
        };
        (generation == target.generation
            && state.binding_generation == target.binding_generation
            && state.source == target.source)
            .then_some(state)
    }

    fn live_parts_mut(
        &mut self,
        target: &LiveTarget,
    ) -> Option<(&mut LiveState, &mut HashMap<String, EmoteInfo>)> {
        self.live_state(target)?;
        let provider = target.source.provider.name();
        match &target.scope {
            LiveScope::Channel(channel) => {
                let scope = self.channels.get_mut(channel)?;
                Some((
                    scope.live.get_mut(provider)?,
                    scope.emotes.entry(provider.into()).or_default(),
                ))
            }
            LiveScope::Global => Some((
                &mut self.global_live.as_mut()?.state,
                self.global.entry(provider.into()).or_default(),
            )),
            LiveScope::Personal(user) => Some((
                &mut self.personal_live.get_mut(user)?.state,
                self.personal
                    .entry(user.clone())
                    .or_default()
                    .entry(provider.into())
                    .or_default(),
            )),
        }
    }

    pub(crate) fn begin_live_epoch(&mut self, target: &LiveTarget, epoch: u64) -> bool {
        self.live_parts_mut(target)
            .is_some_and(|(state, _)| state.begin_epoch(epoch))
    }

    pub(crate) fn live_snapshot_ticket(
        &self,
        target: &LiveTarget,
        epoch: u64,
    ) -> Option<SnapshotTicket> {
        self.live_state(target)?.ticket(epoch)
    }

    pub(crate) fn apply_live_delta(
        &mut self,
        target: &LiveTarget,
        epoch: u64,
        delta: EmoteDelta,
    ) -> ApplyResult {
        let total = self.personal_emote_count;
        let Some((state, emotes)) = self.live_parts_mut(target) else {
            return ApplyResult::default();
        };
        let before = emotes.len();
        let mut result = state.observe(emotes, epoch, delta, Instant::now());
        if matches!(target.scope, LiveScope::Personal(_)) {
            let limit = MAX_PERSONAL_EMOTES_PER_USER
                .min(MAX_PERSONAL_EMOTES.saturating_sub(total.saturating_sub(before)));
            result.changed |= trim_emotes(emotes, limit);
            let after = emotes.len();
            self.personal_emote_count = total.saturating_sub(before) + after;
        }
        result
    }

    pub(crate) fn apply_live_snapshot(
        &mut self,
        target: &LiveTarget,
        ticket: SnapshotTicket,
        emotes: Vec<EmoteInfo>,
        fresh: bool,
    ) -> ApplyResult {
        let total = self.personal_emote_count;
        let Some((state, current)) = self.live_parts_mut(target) else {
            return ApplyResult::default();
        };
        let before = current.len();
        let mut result = state.replace_snapshot(current, ticket, emotes, fresh, Instant::now());
        if matches!(target.scope, LiveScope::Personal(_)) {
            let limit = MAX_PERSONAL_EMOTES_PER_USER
                .min(MAX_PERSONAL_EMOTES.saturating_sub(total.saturating_sub(before)));
            result.changed |= trim_emotes(current, limit);
            let after = current.len();
            self.personal_emote_count = total.saturating_sub(before) + after;
        }
        result
    }

    pub(crate) fn switch_live_source(
        &mut self,
        target: &LiveTarget,
        epoch: u64,
        source: LiveSource,
    ) -> bool {
        let Some((state, emotes)) = self.live_parts_mut(target) else {
            return false;
        };
        if state.ticket(epoch).is_none()
            || source.provider != target.source.provider
            || source.room_id != target.source.room_id
            || source.platform != target.source.platform
            || source == state.source
        {
            return false;
        }
        let changed_catalog = !state.source.same_catalog(&source);
        state.rebind_authoritative(source, Instant::now());
        if changed_catalog {
            let removed = emotes.len();
            emotes.clear();
            match &target.scope {
                LiveScope::Channel(channel) => {
                    self.channels
                        .get_mut(channel)
                        .unwrap()
                        .inline_emotes
                        .remove(target.source.provider.name());
                }
                LiveScope::Personal(_) => {
                    self.personal_emote_count = self.personal_emote_count.saturating_sub(removed)
                }
                LiveScope::Global => {}
            }
        }
        true
    }

    pub(crate) fn channel_emote_count(&self, channel: &ChannelId) -> usize {
        self.channels
            .get(channel)
            .map(|scope| {
                let fetched: usize = scope.emotes.values().map(HashMap::len).sum();
                fetched
                    + scope
                        .inline_emotes
                        .values()
                        .flat_map(|emotes| emotes.values())
                        .filter(|emote| {
                            lookup(&scope.emotes, &emote.provider, &emote.code).is_none()
                        })
                        .count()
            })
            .unwrap_or_default()
    }

    pub(crate) fn insert_channel_emote(&mut self, channel: &ChannelId, emote: EmoteInfo) -> bool {
        let Some(scope) = self.channels.get_mut(channel) else {
            return false;
        };
        let changed = lookup(&scope.inline_emotes, &emote.provider, &emote.code)
            .or_else(|| lookup(&scope.emotes, &emote.provider, &emote.code))
            .is_none_or(|old| old != &emote);
        scope
            .inline_emotes
            .entry(emote.provider.clone())
            .or_default()
            .insert(emote.code.clone(), emote);
        changed
    }

    /// Sender data is public and stays keyed to its owner; pending account-era jobs expire.
    pub(crate) fn set_account(&mut self, user_id: Option<&str>) {
        if self.active_account.as_deref() == user_id {
            return;
        }
        self.active_account = user_id.map(str::to_owned);
        self.twitch_inventory.clear();
        self.twitch_inventory_generation = None;
        for scope in self.channels.values_mut() {
            scope.twitch_followers.clear();
            scope.twitch_follower_generation = None;
        }
        self.whisper_senders.clear();
        self.pending_users.clear();
        self.deferred_users.retain(|_, channel| channel.is_some());
        let deferred = &self.deferred_users;
        self.deferred_user_order
            .retain(|user| deferred.contains_key(user));
    }

    pub(crate) fn begin_twitch_inventory(&mut self, user_id: &str) -> Option<u64> {
        if user_id.is_empty() || self.active_account.as_deref() != Some(user_id) {
            return None;
        }
        self.next_generation = self.next_generation.wrapping_add(1);
        self.twitch_inventory_generation = Some(self.next_generation);
        Some(self.next_generation)
    }

    /// Private entitlements never share storage with public sender emotes.
    pub(crate) fn complete_twitch_inventory(
        &mut self,
        user_id: &str,
        generation: u64,
        emotes: Option<Vec<EmoteInfo>>,
    ) -> bool {
        if self.active_account.as_deref() != Some(user_id)
            || self.twitch_inventory_generation != Some(generation)
        {
            return false;
        }
        self.twitch_inventory_generation = None;
        let Some(emotes) = emotes else {
            return false;
        };
        let inventory = provider_maps(emotes);
        let changed = self.twitch_inventory != inventory;
        self.twitch_inventory = inventory;
        changed
    }

    pub(crate) fn clear_twitch_inventory(&mut self, user_id: &str) -> bool {
        if self.active_account.as_deref() != Some(user_id) {
            return false;
        }
        self.twitch_inventory_generation = None;
        let mut changed = !self.twitch_inventory.is_empty();
        self.twitch_inventory.clear();
        for scope in self.channels.values_mut() {
            scope.twitch_follower_generation = None;
            changed |= !scope.twitch_followers.is_empty();
            scope.twitch_followers.clear();
        }
        changed
    }

    pub(crate) fn twitch_follower_targets(&self) -> Vec<TwitchFollowerTarget> {
        self.channels
            .iter()
            .filter(|(channel, scope)| channel.is_twitch() && scope.live_enabled)
            .filter_map(|(channel, scope)| {
                Some(TwitchFollowerTarget {
                    channel: channel.clone(),
                    room_id: scope.live_room_id.as_ref()?.clone(),
                    generation: scope.live_generation,
                })
            })
            .collect()
    }

    pub(crate) fn begin_twitch_follower_inventory(
        &mut self,
        user_id: &str,
        target: &TwitchFollowerTarget,
    ) -> Option<u64> {
        if user_id.is_empty() || self.active_account.as_deref() != Some(user_id) {
            return None;
        }
        let scope = self.channels.get_mut(&target.channel)?;
        if !target.channel.is_twitch()
            || !scope.live_enabled
            || scope.live_generation != target.generation
            || scope.live_room_id.as_deref() != Some(target.room_id.as_str())
        {
            return None;
        }
        self.next_generation = self.next_generation.wrapping_add(1);
        scope.twitch_follower_generation = Some(self.next_generation);
        Some(self.next_generation)
    }

    pub(crate) fn complete_twitch_follower_inventory(
        &mut self,
        user_id: &str,
        target: &TwitchFollowerTarget,
        generation: u64,
        emotes: Option<Vec<EmoteInfo>>,
    ) -> bool {
        if self.active_account.as_deref() != Some(user_id) {
            return false;
        }
        let Some(scope) = self.channels.get_mut(&target.channel) else {
            return false;
        };
        if !scope.live_enabled
            || scope.live_generation != target.generation
            || scope.live_room_id.as_deref() != Some(target.room_id.as_str())
            || scope.twitch_follower_generation != Some(generation)
        {
            return false;
        }
        scope.twitch_follower_generation = None;
        let Some(emotes) = emotes else {
            return false;
        };
        let inventory = provider_maps(emotes);
        let changed = scope.twitch_followers != inventory;
        scope.twitch_followers = inventory;
        changed
    }

    /// Schedule a known whisper sender through the same bounded queue as chat senders.
    pub(crate) fn queue_whisper_sender(
        &mut self,
        account_user_id: &str,
        sender_user_id: &str,
    ) -> bool {
        if account_user_id.is_empty()
            || self.active_account.as_deref() != Some(account_user_id)
            || sender_user_id.is_empty()
        {
            return false;
        }
        if sender_user_id != account_user_id {
            if self.whisper_senders.len() >= MAX_CACHED_USERS
                && !self.whisper_senders.contains(sender_user_id)
            {
                return false;
            }
            self.whisper_senders.insert(sender_user_id.to_owned());
        }
        self.defer_personal_load(sender_user_id, None);
        true
    }

    fn needs_personal_load(&self, user_id: &str, channel: Option<&ChannelId>) -> bool {
        !user_id.is_empty()
            && !self.pending_users.contains_key(user_id)
            && !self
                .cached_users
                .get(user_id)
                .is_some_and(|retry_at| Instant::now() < *retry_at)
            && match channel {
                Some(channel) => channel.is_twitch() && self.channels.contains_key(channel),
                None => self.active_account.as_deref().is_some_and(|account| {
                    account == user_id || self.whisper_senders.contains(user_id)
                }),
            }
    }

    pub(crate) fn active_user_needing_lookup(&self) -> Option<String> {
        self.active_account
            .as_ref()
            .filter(|user| self.needs_personal_load(user, None))
            .cloned()
    }

    pub(crate) fn defer_personal_load(&mut self, user_id: &str, channel: Option<&ChannelId>) {
        if !self.needs_personal_load(user_id, channel) {
            return;
        }
        if self.deferred_users.contains_key(user_id) {
            if channel.is_none() {
                self.deferred_user_order.retain(|user| user != user_id);
                self.deferred_user_order.push_front(user_id.to_owned());
            }
            if channel.is_none()
                || self
                    .deferred_users
                    .get(user_id)
                    .is_some_and(Option::is_some)
            {
                self.deferred_users
                    .insert(user_id.to_owned(), channel.cloned());
            }
            return;
        }
        if self.deferred_users.len() >= MAX_CACHED_USERS {
            if channel.is_some() {
                return;
            }
            if let Some(last) = self.deferred_user_order.pop_back() {
                self.deferred_users.remove(&last);
            }
        }
        self.deferred_users
            .insert(user_id.to_owned(), channel.cloned());
        if channel.is_none() {
            self.deferred_user_order.push_front(user_id.to_owned());
        } else {
            self.deferred_user_order.push_back(user_id.to_owned());
        }
    }

    pub(crate) fn next_deferred_personal_load(&mut self) -> Option<(String, Option<ChannelId>)> {
        while let Some(user) = self.deferred_user_order.pop_front() {
            let Some(channel) = self.deferred_users.remove(&user) else {
                continue;
            };
            if self.needs_personal_load(&user, channel.as_ref()) {
                return Some((user, channel));
            }
        }
        None
    }

    /// Reserve one bounded lookup for an open channel or the current account's senders.
    pub(crate) fn begin_personal_load(
        &mut self,
        user_id: &str,
        channel: Option<&ChannelId>,
    ) -> Option<u64> {
        if self.pending_users.len() >= MAX_PENDING_USERS
            || !self.needs_personal_load(user_id, channel)
        {
            return None;
        }
        let channel = match channel {
            Some(channel) if channel.is_twitch() && self.channels.contains_key(channel) => {
                Some(channel.clone())
            }
            None if self.active_account.as_deref().is_some_and(|account| {
                account == user_id || self.whisper_senders.contains(user_id)
            }) =>
            {
                None
            }
            _ => return None,
        };
        self.next_generation = self.next_generation.wrapping_add(1);
        self.pending_users.insert(
            user_id.to_owned(),
            UserLookup {
                generation: self.next_generation,
                channel,
                live_binding: self
                    .personal_live
                    .get(user_id)
                    .map(|scope| scope.state.binding()),
            },
        );
        Some(self.next_generation)
    }

    #[cfg(test)]
    pub(crate) fn complete_personal_load(
        &mut self,
        user_id: &str,
        generation: u64,
        emotes: Option<Vec<EmoteInfo>>,
    ) -> Option<UserEmoteUpdate> {
        self.complete_personal_load_with_source(user_id, generation, emotes, None)
    }

    pub(crate) fn complete_personal_load_with_source(
        &mut self,
        user_id: &str,
        generation: u64,
        mut emotes: Option<Vec<EmoteInfo>>,
        source: Option<LiveSource>,
    ) -> Option<UserEmoteUpdate> {
        if self
            .pending_users
            .get(user_id)
            .map(|lookup| lookup.generation)
            != Some(generation)
        {
            return None;
        }
        let pending = self.pending_users.remove(user_id).unwrap();
        if self
            .personal_live
            .get(user_id)
            .map(|scope| scope.state.binding())
            != pending.live_binding
        {
            emotes = None;
        }
        let retry_after = if emotes.is_some() {
            USER_LOOKUP_TTL
        } else {
            Duration::from_secs(30)
        };
        let mut catalog_changed = false;
        if let Some(mut emotes) = emotes {
            emotes.truncate(MAX_PERSONAL_EMOTES_PER_USER);
            let providers = if let Some(source) =
                source.filter(|source| source.provider == Provider::SevenTv)
            {
                match self.personal_live.get_mut(user_id) {
                    Some(scope) => scope.state.rebind_authoritative(source, Instant::now()),
                    None => {
                        self.next_generation = self.next_generation.wrapping_add(1);
                        self.personal_live.insert(
                            user_id.to_owned(),
                            AuxiliaryLive {
                                generation: self.next_generation,
                                state: LiveState::new(source),
                            },
                        );
                    }
                }
                let mut merged = self.personal_live[user_id].state.overlay_snapshot(
                    emotes,
                    self.personal
                        .get(user_id)
                        .and_then(|providers| providers.get("7tv")),
                );
                trim_emotes(&mut merged, MAX_PERSONAL_EMOTES_PER_USER);
                if merged.is_empty() {
                    ProviderEmotes::new()
                } else {
                    HashMap::from([("7tv".into(), merged)])
                }
            } else {
                self.personal_live.remove(user_id);
                provider_maps(emotes)
            };
            catalog_changed = if providers.is_empty() {
                self.personal
                    .get(user_id)
                    .is_some_and(|current| current.values().any(|emotes| !emotes.is_empty()))
            } else {
                self.personal.get(user_id) != Some(&providers)
            };
            if let Some(old) = self.personal.remove(user_id) {
                self.personal_emote_count -= old.values().map(HashMap::len).sum::<usize>();
            }
            if !providers.is_empty() {
                self.personal_emote_count += providers.values().map(HashMap::len).sum::<usize>();
                self.personal.insert(user_id.to_owned(), providers);
            }
        }
        self.cached_users
            .insert(user_id.to_owned(), Instant::now() + retry_after);
        self.cached_user_order.retain(|cached| cached != user_id);
        self.cached_user_order.push_back(user_id.to_owned());
        let mut evicted_users = Vec::new();
        while self.cached_users.len() > MAX_CACHED_USERS
            || self.personal_emote_count > MAX_PERSONAL_EMOTES
        {
            let Some(oldest) = self.cached_user_order.pop_front() else {
                break;
            };
            if self.active_account.as_deref() == Some(oldest.as_str()) {
                self.cached_user_order.push_back(oldest);
                continue;
            }
            self.cached_users.remove(&oldest);
            self.personal_live.remove(&oldest);
            if let Some(old) = self.personal.remove(&oldest) {
                self.personal_emote_count -= old.values().map(HashMap::len).sum::<usize>();
                catalog_changed = true;
            }
            evicted_users.push(oldest);
        }
        Some(UserEmoteUpdate {
            evicted_users,
            catalog_changed,
        })
    }

    pub(crate) fn resolve(
        &self,
        channel: &ChannelId,
        sender_user_id: &str,
        code: &str,
    ) -> Option<&EmoteInfo> {
        if channel.is_twitch() && self.active_account.as_deref() == Some(sender_user_id) {
            if let Some(emote) = self
                .channels
                .get(channel)
                .and_then(|scope| lookup(&scope.twitch_followers, "twitch", code))
            {
                return Some(emote);
            }
            if let Some(emote) = lookup(&self.twitch_inventory, "twitch", code) {
                return Some(emote);
            }
        }
        if let Some(personal) = self
            .personal
            .get(sender_user_id)
            .filter(|_| channel.is_twitch())
        {
            if let Some(emote) = lookup(personal, "7tv", code) {
                return Some(emote);
            }
        }
        if let Some(scope) = self.channels.get(channel) {
            for provider in ["kick", "ffz", "bttv", "7tv"] {
                if let Some(emote) = lookup(&scope.inline_emotes, provider, code)
                    .or_else(|| lookup(&scope.emotes, provider, code))
                {
                    return Some(emote);
                }
            }
        }
        for provider in ["ffz", "bttv", "7tv", "twitch"] {
            if let Some(emote) = lookup(&self.global, provider, code) {
                return Some(emote);
            }
        }
        None
    }

    pub(crate) fn twitch_for_user(&self, user_id: &str, code: &str) -> Option<&EmoteInfo> {
        self.twitch_inventory
            .get("twitch")
            .filter(|_| !user_id.is_empty() && self.active_account.as_deref() == Some(user_id))
            .and_then(|emotes| emotes.get(code))
            .or_else(|| self.twitch_global(code))
    }

    pub(crate) fn twitch_for_channel_user(
        &self,
        channel: &ChannelId,
        user_id: &str,
        code: &str,
    ) -> Option<&EmoteInfo> {
        self.channels
            .get(channel)
            .filter(|_| channel.is_twitch() && self.active_account.as_deref() == Some(user_id))
            .and_then(|scope| lookup(&scope.twitch_followers, "twitch", code))
            .or_else(|| self.twitch_for_user(user_id, code))
    }

    pub(crate) fn twitch_global(&self, code: &str) -> Option<&EmoteInfo> {
        lookup(&self.global, "twitch", code)
    }

    /// Remote history only needs the requested channel's definitions.
    pub(crate) fn snapshot_for(&self, channel: &ChannelId) -> Self {
        Self {
            global: self.global.clone(),
            channels: self
                .channels
                .get(channel)
                .map(|scope| {
                    HashMap::from([(
                        channel.clone(),
                        ChannelEmotes {
                            emotes: scope.emotes.clone(),
                            inline_emotes: scope.inline_emotes.clone(),
                            ..ChannelEmotes::default()
                        },
                    )])
                })
                .unwrap_or_default(),
            personal: self.personal.clone(),
            ..Self::default()
        }
    }

    pub(crate) fn catalog(&self) -> Vec<EmoteCatalogEntry> {
        let mut catalog = Vec::new();
        let mut append = |providers: &ProviderEmotes,
                          channel: Option<&ChannelId>,
                          user_id: Option<&str>,
                          overrides: Option<&ProviderEmotes>| {
            for emote in providers.values().flat_map(|emotes| emotes.values()) {
                if overrides.is_some_and(|overrides| {
                    lookup(overrides, &emote.provider, &emote.code).is_some()
                }) {
                    continue;
                }
                catalog.push(EmoteCatalogEntry {
                    id: emote.id.clone(),
                    code: emote.code.clone(),
                    provider: emote.provider.clone(),
                    url: emote.url_1x.clone(),
                    url_hd: emote.url_4x.clone().or_else(|| emote.url_2x.clone()),
                    zero_width: emote.zero_width,
                    scope: if user_id.is_some() {
                        "personal"
                    } else if channel.is_some() {
                        "channel"
                    } else {
                        "global"
                    }
                    .to_owned(),
                    channel: channel.cloned(),
                    user_id: user_id.map(|id| UserId(id.to_owned())),
                });
            }
        };
        append(&self.global, None, None, None);
        for (channel, scope) in &self.channels {
            append(
                &scope.emotes,
                Some(channel),
                None,
                Some(&scope.inline_emotes),
            );
            append(&scope.inline_emotes, Some(channel), None, None);
            if let Some(owner) = &self.active_account {
                append(&scope.twitch_followers, Some(channel), Some(owner), None);
            }
        }
        for (user_id, emotes) in &self.personal {
            append(emotes, None, Some(user_id), None);
        }
        if let Some(owner) = &self.active_account {
            append(&self.twitch_inventory, None, Some(owner), None);
        }
        catalog
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emote(provider: &str, id: &str, code: &str) -> EmoteInfo {
        EmoteInfo {
            provider: provider.into(),
            zero_width: false,
            id: id.into(),
            code: code.into(),
            url_1x: format!("https://example.com/{id}"),
            url_2x: None,
            url_4x: None,
        }
    }

    fn load(registry: &mut EmoteRegistry, channel: &ChannelId, emotes: Vec<EmoteInfo>) {
        registry.open_channel(channel);
        let generation = registry.begin_channel_load(channel, "room", true).unwrap();
        for (provider, emotes) in provider_maps(emotes) {
            assert!(registry.replace_channel(
                channel,
                generation,
                &provider,
                Some(emotes.into_values().collect())
            ));
        }
        assert!(registry.complete_channel_load(channel, generation));
    }

    fn live_source(set_id: &str) -> LiveSource {
        LiveSource {
            provider: crate::runtime::live_emotes::Provider::SevenTv,
            room_id: "123".into(),
            platform: "TWITCH".into(),
            user_id: Some("user".into()),
            set_id: Some(set_id.into()),
            connection_index: Some(0),
        }
    }

    #[test]
    fn global_refresh_failures_empty_results_and_older_generations_remain_distinct() {
        let mut registry = EmoteRegistry::default();
        let first = registry.begin_global_load();
        assert!(registry.replace_global_result(
            first,
            "ffz",
            Some(vec![emote("ffz", "old", "Old")])
        ));
        assert!(!registry.replace_global_result(first, "ffz", None));
        assert_eq!(
            registry
                .resolve(&ChannelId::new("room"), "", "Old")
                .unwrap()
                .id,
            "old"
        );
        assert!(!registry.replace_global_result(
            first,
            "ffz",
            Some(vec![emote("ffz", "old", "Old")])
        ));
        let second = registry.begin_global_load();
        assert!(registry.replace_global_result(second, "ffz", Some(Vec::new())));
        assert!(!registry.replace_global_result(
            first,
            "ffz",
            Some(vec![emote("ffz", "stale", "Old")])
        ));
        assert!(registry.catalog().is_empty());
    }

    fn bind_personal(registry: &mut EmoteRegistry, user: &str, set: &str, emotes: Vec<EmoteInfo>) {
        registry.set_account(Some(user));
        registry.cached_users.remove(user);
        let generation = registry.begin_personal_load(user, None).unwrap();
        let mut source = live_source(set);
        source.room_id = user.into();
        source.connection_index = None;
        registry
            .complete_personal_load_with_source(user, generation, Some(emotes), Some(source))
            .unwrap();
    }

    #[test]
    fn personal_live_updates_preserve_owner_scope_and_stale_profile_refreshes_cannot_restore_old_sets(
    ) {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("room");
        bind_personal(
            &mut registry,
            "alice",
            "old-set",
            vec![emote("7tv", "old", "Old")],
        );
        let target = registry.live_targets().pop().unwrap();
        assert!(registry.begin_live_epoch(&target, 1));
        registry.cached_users.remove("alice");
        let stale_profile = registry.begin_personal_load("alice", None).unwrap();
        let mut next_source = target.source.clone();
        next_source.set_id = Some("new-set".into());
        assert!(registry.switch_live_source(&target, 1, next_source));
        assert!(
            !registry
                .complete_personal_load_with_source(
                    "alice",
                    stale_profile,
                    Some(vec![emote("7tv", "old", "Old")]),
                    Some(target.source.clone())
                )
                .unwrap()
                .catalog_changed
        );
        assert!(registry.resolve(&channel, "alice", "Old").is_none());
        assert!(
            !registry
                .apply_live_delta(
                    &target,
                    1,
                    EmoteDelta::Upsert {
                        emote: emote("7tv", "late", "Late"),
                        old_code: None,
                        preserve_overlay: false,
                    }
                )
                .changed
        );
        let current = registry.live_targets().pop().unwrap();
        assert!(registry.begin_live_epoch(&current, 2));
        let ticket = registry.live_snapshot_ticket(&current, 2).unwrap();
        assert!(
            registry
                .apply_live_snapshot(&current, ticket, vec![emote("7tv", "new", "New")], true)
                .changed
        );
        assert!(registry.resolve(&channel, "alice", "New").is_some());
        assert!(registry.resolve(&channel, "bob", "New").is_none());
        assert!(registry
            .resolve(&ChannelId::kick("room"), "alice", "New")
            .is_none());
        assert_eq!(registry.personal_emote_count, 1);
    }

    #[test]
    fn personal_empty_live_snapshot_remains_empty_across_repeated_profile_refreshes() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("room");
        bind_personal(
            &mut registry,
            "alice",
            "set",
            vec![emote("7tv", "old", "Old")],
        );
        let target = registry.live_targets().pop().unwrap();
        assert!(registry.begin_live_epoch(&target, 1));
        let ticket = registry.live_snapshot_ticket(&target, 1).unwrap();
        assert!(
            registry
                .apply_live_snapshot(&target, ticket, Vec::new(), true)
                .changed
        );
        for _ in 0..2 {
            bind_personal(
                &mut registry,
                "alice",
                "set",
                vec![emote("7tv", "old", "Old")],
            );
            assert!(!registry.personal.contains_key("alice"));
            assert!(registry.resolve(&channel, "alice", "Old").is_none());
            assert_eq!(registry.personal_emote_count, 0);
        }
    }

    #[test]
    fn cached_channel_source_cannot_undo_an_authoritative_set_switch() {
        for (previous, next) in [
            (Some("old"), Some("new")),
            (Some("old"), None),
            (None, Some("new")),
        ] {
            let mut registry = EmoteRegistry::default();
            let channel = ChannelId::new("room");
            registry.open_channel(&channel);
            let generation = registry.begin_channel_load(&channel, "123", false).unwrap();
            let mut old_source = live_source("old");
            old_source.set_id = previous.map(str::to_owned);
            assert!(registry.replace_channel_with_source(
                &channel,
                generation,
                "7tv",
                Some(vec![emote("7tv", "old", "Old")]),
                Some(old_source.clone()),
                None
            ));
            registry.complete_channel_load(&channel, generation);
            let target = registry.live_targets().pop().unwrap();
            assert!(registry.begin_live_epoch(&target, 1));
            let mut current_source = old_source.clone();
            current_source.set_id = next.map(str::to_owned);
            assert!(registry.switch_live_source(&target, 1, current_source));
            let current = registry.live_targets().pop().unwrap();
            assert!(registry.begin_live_epoch(&current, 2));
            let ticket = registry.live_snapshot_ticket(&current, 2).unwrap();
            registry.apply_live_snapshot(&current, ticket, Vec::new(), true);

            let refresh = registry.begin_channel_load(&channel, "123", true).unwrap();
            let binding = registry.live_binding(&channel, refresh, "7tv");
            assert!(registry.replace_channel_with_source(
                &channel,
                refresh,
                "7tv",
                Some(vec![emote("7tv", "old", "Old")]),
                Some(old_source),
                binding.clone()
            ));
            assert_eq!(registry.live_binding(&channel, refresh, "7tv"), binding);
            assert!(registry.resolve(&channel, "", "Old").is_none());
            assert!(registry.complete_channel_load(&channel, refresh));
        }
    }

    #[test]
    fn cached_global_source_cannot_restore_a_deleted_set_and_new_sets_still_load() {
        let mut registry = EmoteRegistry::default();
        let mut old_source = live_source("old");
        old_source.user_id = None;
        old_source.room_id = "global".into();
        old_source.platform = "GLOBAL".into();
        old_source.connection_index = None;
        registry.replace_global_live(old_source.clone(), vec![emote("7tv", "old", "Old")], None);
        let target = registry.live_targets().pop().unwrap();
        assert!(registry.begin_live_epoch(&target, 1));
        let mut deleted = old_source.clone();
        deleted.set_id = None;
        assert!(registry.switch_live_source(&target, 1, deleted));
        let current = registry.live_targets().pop().unwrap();
        assert!(registry.begin_live_epoch(&current, 2));
        let ticket = registry.live_snapshot_ticket(&current, 2).unwrap();
        registry.apply_live_snapshot(&current, ticket, Vec::new(), true);
        let binding = registry.global_live_binding();
        assert!(!registry.replace_global_live(
            old_source.clone(),
            vec![emote("7tv", "old", "Old")],
            binding.clone()
        ));
        assert_eq!(registry.global_live_binding(), binding);

        let mut replacement = old_source.clone();
        replacement.set_id = Some("new-global".into());
        assert!(registry.replace_global_live(
            replacement,
            vec![emote("7tv", "new", "New")],
            binding
        ));
        let binding = registry.global_live_binding();
        assert!(!registry.replace_global_live(
            old_source,
            vec![emote("7tv", "old", "Old")],
            binding
        ));
        assert!(registry
            .resolve(&ChannelId::new("room"), "", "New")
            .is_some());
        assert!(registry
            .resolve(&ChannelId::new("room"), "", "Old")
            .is_none());
    }

    #[test]
    fn personal_live_snapshots_and_deltas_keep_the_existing_memory_bounds() {
        let mut registry = EmoteRegistry::default();
        bind_personal(&mut registry, "alice", "set", Vec::new());
        let target = registry.live_targets().pop().unwrap();
        assert!(registry.begin_live_epoch(&target, 1));
        let ticket = registry.live_snapshot_ticket(&target, 1).unwrap();
        let emotes = (0..MAX_PERSONAL_EMOTES_PER_USER + 10)
            .map(|id| emote("7tv", &id.to_string(), &format!("Code{id:04}")))
            .collect();
        assert!(
            registry
                .apply_live_snapshot(&target, ticket, emotes, true)
                .changed
        );
        assert_eq!(registry.personal_emote_count, MAX_PERSONAL_EMOTES_PER_USER);
        registry.apply_live_delta(
            &target,
            1,
            EmoteDelta::Upsert {
                emote: emote("7tv", "extra", "Extra"),
                old_code: None,
                preserve_overlay: false,
            },
        );
        assert_eq!(registry.personal_emote_count, MAX_PERSONAL_EMOTES_PER_USER);
        registry.apply_live_delta(
            &target,
            1,
            EmoteDelta::Remove {
                id: "0".into(),
                code: Some("Code0000".into()),
            },
        );
        assert_eq!(
            registry.personal_emote_count,
            MAX_PERSONAL_EMOTES_PER_USER - 1
        );
    }

    #[test]
    fn global_live_removals_survive_manual_refresh_and_stale_bindings_are_rejected() {
        let mut registry = EmoteRegistry::default();
        let mut source = live_source("global-set");
        source.user_id = None;
        source.room_id = "global".into();
        source.platform = "GLOBAL".into();
        source.connection_index = None;
        assert!(registry.replace_global_live(
            source.clone(),
            vec![emote("7tv", "old", "Old")],
            None
        ));
        let binding = registry.global_live_binding();
        let target = registry.live_targets().pop().unwrap();
        assert!(registry.begin_live_epoch(&target, 1));
        assert!(
            registry
                .apply_live_delta(
                    &target,
                    1,
                    EmoteDelta::Remove {
                        id: "old".into(),
                        code: Some("Old".into())
                    }
                )
                .changed
        );
        registry.replace_global_live(
            source.clone(),
            vec![emote("7tv", "old", "Old")],
            binding.clone(),
        );
        assert!(registry
            .resolve(&ChannelId::new("room"), "", "Old")
            .is_none());
        let confirmed = registry.live_snapshot_ticket(&target, 1).unwrap();
        registry.apply_live_snapshot(&target, confirmed, Vec::new(), true);
        assert!(!registry.replace_global_live(
            source.clone(),
            vec![emote("7tv", "old", "Old")],
            binding.clone(),
        ));
        assert!(registry
            .resolve(&ChannelId::new("room"), "", "Old")
            .is_none());
        let mut next_source = source.clone();
        next_source.set_id = Some("replacement".into());
        assert!(registry.switch_live_source(&target, 1, next_source));
        assert!(!registry.replace_global_live(
            source,
            vec![emote("7tv", "stale", "Stale")],
            binding
        ));
        assert!(registry
            .resolve(&ChannelId::new("room"), "", "Stale")
            .is_none());
    }

    #[test]
    fn follower_entitlements_require_both_the_account_and_channel() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("room");
        let other = ChannelId::new("other");
        registry.set_account(Some("alice"));
        registry.open_channel(&channel);
        registry.begin_channel_load(&channel, "123", false).unwrap();
        let target = registry.twitch_follower_targets().pop().unwrap();
        let generation = registry
            .begin_twitch_follower_inventory("alice", &target)
            .unwrap();
        assert!(registry.complete_twitch_follower_inventory(
            "alice",
            &target,
            generation,
            Some(vec![emote("twitch", "follower", "FollowWave")])
        ));
        assert_eq!(
            registry
                .resolve(&channel, "alice", "FollowWave")
                .unwrap()
                .id,
            "follower"
        );
        assert!(registry.resolve(&other, "alice", "FollowWave").is_none());
        assert!(registry.resolve(&channel, "bob", "FollowWave").is_none());
        assert!(registry.twitch_for_user("alice", "FollowWave").is_none());
        assert!(registry
            .twitch_for_channel_user(&channel, "alice", "FollowWave")
            .is_some());
        assert!(registry
            .twitch_for_channel_user(&other, "alice", "FollowWave")
            .is_none());
        let catalog = registry.catalog();
        assert_eq!(catalog.len(), 1);
        assert!(catalog[0].is_available_for(&channel, "alice"));
        assert!(!catalog[0].is_available_for(&other, "alice"));
        assert!(!catalog[0].is_available_for(&channel, "bob"));
        assert!(!catalog[0].is_available_for(&ChannelId::mentions(), "alice"));
        registry.set_account(Some("bob"));
        assert!(registry.catalog().is_empty());
    }

    #[test]
    fn follower_loads_reject_old_accounts_rooms_and_refresh_generations() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("room");
        registry.set_account(Some("alice"));
        registry.open_channel(&channel);
        registry.begin_channel_load(&channel, "123", false).unwrap();
        let first = registry.twitch_follower_targets().pop().unwrap();
        let first_request = registry
            .begin_twitch_follower_inventory("alice", &first)
            .unwrap();
        let current_request = registry
            .begin_twitch_follower_inventory("alice", &first)
            .unwrap();
        let response = || Some(vec![emote("twitch", "follower", "FollowWave")]);
        assert!(!registry.complete_twitch_follower_inventory(
            "alice",
            &first,
            first_request,
            response()
        ));
        assert!(registry.complete_twitch_follower_inventory(
            "alice",
            &first,
            current_request,
            response()
        ));
        let first_request = registry
            .begin_twitch_follower_inventory("alice", &first)
            .unwrap();
        registry.reconnect_channel(&channel);
        assert!(!registry.complete_twitch_follower_inventory(
            "alice",
            &first,
            first_request,
            response()
        ));
        registry.begin_channel_load(&channel, "123", false).unwrap();
        let current = registry.twitch_follower_targets().pop().unwrap();
        assert_ne!(first, current);
        let current_request = registry
            .begin_twitch_follower_inventory("alice", &current)
            .unwrap();
        registry.close_channel(&channel);
        registry.open_channel(&channel);
        registry.begin_channel_load(&channel, "456", false).unwrap();
        assert!(!registry.complete_twitch_follower_inventory(
            "alice",
            &current,
            current_request,
            response()
        ));
        let current = registry.twitch_follower_targets().pop().unwrap();
        let current_request = registry
            .begin_twitch_follower_inventory("alice", &current)
            .unwrap();
        registry.set_account(Some("bob"));
        assert!(!registry.complete_twitch_follower_inventory(
            "alice",
            &current,
            current_request,
            response()
        ));
        assert!(registry.catalog().is_empty());
    }

    #[test]
    fn losing_emote_scope_clears_follower_entitlements_and_pending_generations() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("room");
        registry.set_account(Some("alice"));
        registry.open_channel(&channel);
        registry.begin_channel_load(&channel, "123", false).unwrap();
        registry.replace_global("twitch", vec![emote("twitch", "global", "Kappa")]);
        let target = registry.twitch_follower_targets().pop().unwrap();
        let generation = registry
            .begin_twitch_follower_inventory("alice", &target)
            .unwrap();
        assert!(registry.complete_twitch_follower_inventory(
            "alice",
            &target,
            generation,
            Some(vec![emote("twitch", "follower", "FollowWave")])
        ));
        let generation = registry
            .begin_twitch_follower_inventory("alice", &target)
            .unwrap();
        assert!(registry.clear_twitch_inventory("alice"));
        assert!(!registry.complete_twitch_follower_inventory(
            "alice",
            &target,
            generation,
            Some(vec![emote("twitch", "follower", "FollowWave")])
        ));
        assert!(!registry.clear_twitch_inventory("alice"));
        assert_eq!(registry.catalog().len(), 1);
        assert_eq!(registry.catalog()[0].code, "Kappa");
    }

    #[test]
    fn live_set_switch_rejects_old_socket_and_inflight_manual_snapshot() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("room");
        registry.open_channel(&channel);
        let first = registry.begin_channel_load(&channel, "123", false).unwrap();
        let old_source = live_source("old-set");
        assert!(registry.replace_channel_with_source(
            &channel,
            first,
            "7tv",
            Some(vec![emote("7tv", "old", "Old")]),
            Some(old_source.clone()),
            None
        ));
        registry.complete_channel_load(&channel, first);

        let refresh = registry.begin_channel_load(&channel, "123", true).unwrap();
        let request_source = registry.live_binding(&channel, refresh, "7tv");
        let old_target = registry.live_targets().pop().unwrap();
        assert!(registry.begin_live_epoch(&old_target, 1));
        let old_ticket = registry.live_snapshot_ticket(&old_target, 1).unwrap();
        let new_source = live_source("new-set");
        assert!(registry.switch_live_source(&old_target, 1, new_source.clone()));
        assert!(registry.resolve(&channel, "", "Old").is_none());

        assert!(registry.replace_channel_with_source(
            &channel,
            refresh,
            "7tv",
            Some(vec![emote("7tv", "old", "Old")]),
            Some(old_source),
            request_source
        ));
        assert!(registry.resolve(&channel, "", "Old").is_none());
        assert_eq!(
            registry
                .live_binding(&channel, refresh, "7tv")
                .map(|binding| binding.source),
            Some(new_source)
        );
        assert!(
            !registry
                .apply_live_snapshot(
                    &old_target,
                    old_ticket,
                    vec![emote("7tv", "stale", "Stale")],
                    true
                )
                .changed
        );
        assert!(
            !registry
                .apply_live_delta(
                    &old_target,
                    1,
                    EmoteDelta::Upsert {
                        emote: emote("7tv", "stale", "Stale"),
                        old_code: None,
                        preserve_overlay: false,
                    }
                )
                .changed
        );

        let current = registry.live_targets().pop().unwrap();
        assert!(registry.begin_live_epoch(&current, 1));
        let ticket = registry.live_snapshot_ticket(&current, 1).unwrap();
        assert!(
            registry
                .apply_live_snapshot(&current, ticket, vec![emote("7tv", "new", "New")], true)
                .changed
        );
        assert_eq!(registry.resolve(&channel, "", "New").unwrap().id, "new");
        // Returning to the same set is a new lifetime, even on the same socket.
        assert!(registry.switch_live_source(&current, 1, old_target.source.clone()));
        let returned = registry.live_targets().pop().unwrap();
        assert_eq!(returned.source, old_target.source);
        assert_ne!(returned.binding_generation, old_target.binding_generation);
        assert!(registry.begin_live_epoch(&returned, 1));
        assert!(
            !registry
                .apply_live_snapshot(
                    &old_target,
                    old_ticket,
                    vec![emote("7tv", "late", "Late")],
                    true
                )
                .changed
        );
        registry.replace_channel_with_source(
            &channel,
            refresh,
            "7tv",
            Some(vec![emote("7tv", "late", "Late")]),
            Some(old_target.source.clone()),
            Some(LiveBinding {
                generation: old_target.binding_generation,
                source: old_target.source.clone(),
            }),
        );
        assert!(registry.resolve(&channel, "", "Late").is_none());
        registry.close_channel(&channel);
        registry.open_channel(&channel);
        let rejoined = registry.begin_channel_load(&channel, "123", false).unwrap();
        assert_ne!(rejoined, refresh);
        assert!(
            !registry
                .apply_live_snapshot(&current, ticket, vec![emote("7tv", "late", "Late")], true)
                .changed
        );
        assert!(registry.resolve(&channel, "", "Late").is_none());
    }

    #[test]
    fn same_source_manual_reload_keeps_removals_and_reconnect_pauses_delivery() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("room");
        registry.open_channel(&channel);
        let first = registry.begin_channel_load(&channel, "123", false).unwrap();
        let source = live_source("set");
        registry.replace_channel_with_source(
            &channel,
            first,
            "7tv",
            Some(vec![emote("7tv", "id", "Gone")]),
            Some(source.clone()),
            None,
        );
        registry.complete_channel_load(&channel, first);
        let target = registry.live_targets().pop().unwrap();
        registry.begin_live_epoch(&target, 1);
        assert!(
            registry
                .apply_live_delta(
                    &target,
                    1,
                    EmoteDelta::Remove {
                        id: "id".into(),
                        code: Some("Gone".into())
                    }
                )
                .changed
        );
        registry.reconnect_channel(&channel);
        assert!(registry.live_targets().is_empty());
        assert!(
            !registry
                .apply_live_delta(
                    &target,
                    1,
                    EmoteDelta::Upsert {
                        emote: emote("7tv", "old", "Old"),
                        old_code: None,
                        preserve_overlay: false,
                    }
                )
                .changed
        );
        let refresh = registry.begin_channel_load(&channel, "123", false).unwrap();
        let expected = registry.live_binding(&channel, refresh, "7tv");
        registry.replace_channel_with_source(
            &channel,
            refresh,
            "7tv",
            Some(vec![emote("7tv", "id", "Gone")]),
            Some(source),
            expected,
        );
        assert!(registry.resolve(&channel, "", "Gone").is_none());
    }

    #[test]
    fn manual_metadata_refresh_keeps_the_live_room_lifetime() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("room");
        registry.open_channel(&channel);
        let first = registry.begin_channel_load(&channel, "123", false).unwrap();
        registry.replace_channel_with_source(
            &channel,
            first,
            "7tv",
            Some(Vec::new()),
            Some(live_source("set")),
            None,
        );
        registry.complete_channel_load(&channel, first);
        let before = registry.live_targets().pop().unwrap();
        let refresh = registry.begin_channel_load(&channel, "123", true).unwrap();
        assert_ne!(first, refresh);
        assert_eq!(registry.live_targets().pop().unwrap(), before);
    }

    #[test]
    fn channel_collisions_are_isolated_and_channel_emotes_override_globals() {
        let mut registry = EmoteRegistry::default();
        let a = ChannelId::new("a");
        let b = ChannelId::new("b");
        registry.replace_global("ffz", vec![emote("ffz", "global", "Hello")]);
        load(
            &mut registry,
            &a,
            vec![emote("7tv", "a", "Hello"), emote("7tv", "only-a", "OnlyA")],
        );
        load(&mut registry, &b, vec![emote("7tv", "b", "Hello")]);
        assert_eq!(registry.resolve(&a, "", "Hello").unwrap().id, "a");
        assert_eq!(registry.resolve(&b, "", "Hello").unwrap().id, "b");
        assert!(registry.resolve(&b, "", "OnlyA").is_none());
        assert_eq!(
            registry
                .resolve(&ChannelId::new("c"), "", "Hello")
                .unwrap()
                .id,
            "global"
        );
    }

    #[test]
    fn fetched_sets_cannot_overwrite_emotes_learned_from_live_messages() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::kick("room");
        registry.open_channel(&channel);
        let generation = registry.begin_channel_load(&channel, "42", false).unwrap();
        let inline = emote("kick", "inline", "Wave");
        assert!(registry.insert_channel_emote(&channel, inline.clone()));
        assert!(registry.replace_channel(
            &channel,
            generation,
            "kick",
            Some(vec![
                emote("kick", "old", "Wave"),
                emote("kick", "fetched", "Other"),
            ])
        ));
        assert_eq!(registry.resolve(&channel, "", "Wave"), Some(&inline));
        assert_eq!(registry.channel_emote_count(&channel), 2);
        let catalog = registry.catalog();
        let waves: Vec<_> = catalog
            .iter()
            .filter(|entry| entry.code == "Wave")
            .collect();
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0].id, "inline");
        assert_eq!(
            registry
                .snapshot_for(&channel)
                .resolve(&channel, "", "Wave"),
            Some(&inline)
        );

        assert!(registry.complete_channel_load(&channel, generation));
        let generation = registry.begin_channel_load(&channel, "42", true).unwrap();
        assert!(registry.replace_channel(&channel, generation, "kick", Some(Vec::new())));
        assert_eq!(registry.resolve(&channel, "", "Wave"), Some(&inline));
        assert!(registry.resolve(&channel, "", "Other").is_none());
        assert_eq!(registry.channel_emote_count(&channel), 1);
        assert!(registry
            .resolve(&ChannelId::kick("other"), "", "Wave")
            .is_none());
        registry.close_channel(&channel);
        registry.open_channel(&channel);
        assert!(registry.resolve(&channel, "", "Wave").is_none());
    }

    #[test]
    fn unchanged_normal_url_does_not_hide_hd_metadata_updates() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::kick("room");
        registry.open_channel(&channel);
        let mut info = emote("kick", "same", "Wave");
        assert!(registry.insert_channel_emote(&channel, info.clone()));
        assert!(!registry.insert_channel_emote(&channel, info.clone()));
        info.url_4x = Some("https://example.com/new-hd".into());
        assert!(registry.insert_channel_emote(&channel, info));
        assert_eq!(
            registry.catalog()[0].url_hd.as_deref(),
            Some("https://example.com/new-hd")
        );
    }

    #[test]
    fn runtime_and_catalog_use_the_same_provider_and_scope_priority() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("a");
        for provider in ["twitch", "7tv", "bttv", "ffz"] {
            registry.replace_global(
                provider,
                vec![emote(provider, &format!("global-{provider}"), "Same")],
            );
        }
        load(
            &mut registry,
            &channel,
            ["7tv", "bttv", "ffz"]
                .map(|provider| emote(provider, provider, "Same"))
                .into(),
        );
        assert_eq!(registry.resolve(&channel, "42", "Same").unwrap().id, "ffz");
        registry.set_account(Some("42"));
        let generation = registry.begin_personal_load("42", None).unwrap();
        assert!(registry
            .complete_personal_load(
                "42",
                generation,
                Some(vec![emote("7tv", "personal", "Same")])
            )
            .is_some());
        let generation = registry.begin_twitch_inventory("42").unwrap();
        assert!(registry.complete_twitch_inventory(
            "42",
            generation,
            Some(vec![emote("twitch", "subscription", "Same")])
        ));
        for sender in ["42", "other", ""] {
            let catalog = registry.catalog();
            let selected = catalog
                .iter()
                .filter(|entry| entry.is_available_for(&channel, sender))
                .max_by_key(|entry| entry.resolution_priority())
                .unwrap();
            assert_eq!(
                registry.resolve(&channel, sender, "Same").unwrap().url_1x,
                selected.url
            );
        }
    }

    #[test]
    fn native_inventory_belongs_to_the_account_without_erasing_public_sender_sets() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("room");
        registry.set_account(Some("alice"));
        let generation = registry.begin_personal_load("alice", None).unwrap();
        registry.complete_personal_load(
            "alice",
            generation,
            Some(vec![emote("7tv", "public", "PersonalWave")]),
        );
        let generation = registry.begin_twitch_inventory("alice").unwrap();
        assert!(registry.complete_twitch_inventory(
            "alice",
            generation,
            Some(vec![emote("twitch", "subscription", "SubLove")])
        ));
        assert!(registry.resolve(&channel, "alice", "SubLove").is_some());
        assert!(registry.resolve(&channel, "bob", "SubLove").is_none());
        assert!(registry
            .resolve(&ChannelId::kick("room"), "alice", "SubLove")
            .is_none());
        let entry = registry
            .catalog()
            .into_iter()
            .find(|emote| emote.code == "SubLove")
            .unwrap();
        assert!(entry.is_available_for(&channel, "alice"));
        assert!(!entry.is_available_for(&channel, "bob"));

        let old_generation = registry.begin_twitch_inventory("alice").unwrap();
        registry.set_account(Some("bob"));
        assert!(registry.twitch_for_user("alice", "SubLove").is_none());
        assert!(registry
            .resolve(&channel, "alice", "PersonalWave")
            .is_some());
        registry.set_account(Some("alice"));
        assert!(!registry.complete_twitch_inventory(
            "alice",
            old_generation,
            Some(vec![emote("twitch", "stale", "SubLove")])
        ));
        registry.set_account(None);
        assert!(registry
            .catalog()
            .iter()
            .all(|entry| entry.provider != "twitch"));
        assert!(registry
            .resolve(&channel, "alice", "PersonalWave")
            .is_some());
    }

    #[test]
    fn departed_rejoined_and_superseded_channel_loads_cannot_restore_old_emotes() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("a");
        registry.open_channel(&channel);
        let first = registry
            .begin_channel_load(&channel, "room", false)
            .unwrap();
        assert!(registry
            .begin_channel_load(&channel, "room", false)
            .is_none());
        registry.close_channel(&channel);
        assert!(!registry.replace_channel(
            &channel,
            first,
            "ffz",
            Some(vec![emote("ffz", "old", "Same")])
        ));
        registry.open_channel(&channel);
        let second = registry
            .begin_channel_load(&channel, "room", false)
            .unwrap();
        assert_ne!(first, second);
        assert!(!registry.replace_channel(&channel, first, "ffz", Some(vec![])));
        assert!(registry.replace_channel(
            &channel,
            second,
            "ffz",
            Some(vec![emote("ffz", "current", "Same")])
        ));
        assert!(registry
            .begin_channel_load(&channel, "room", true)
            .is_none());
        let third = registry
            .begin_channel_load(&channel, "replacement-room", true)
            .unwrap();
        assert!(registry.replace_channel(&channel, third, "ffz", Some(vec![])));
        assert!(registry.resolve(&channel, "", "Same").is_none());
        assert!(!registry.complete_channel_load(&channel, second));
        assert!(registry.complete_channel_load(&channel, third));
        assert!(!registry.complete_channel_load(&channel, third));
        assert!(registry
            .begin_channel_load(&channel, "replacement-room", false)
            .is_none());
    }

    #[test]
    fn reconnects_allow_refresh_without_dropping_the_current_catalog() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("a");
        load(
            &mut registry,
            &channel,
            vec![emote("ffz", "current", "Same")],
        );
        let pending = registry.begin_channel_load(&channel, "room", true).unwrap();
        registry.reconnect_channel(&channel);
        assert_eq!(
            registry.resolve(&channel, "", "Same").unwrap().id,
            "current"
        );
        assert!(!registry.replace_channel(&channel, pending, "ffz", Some(vec![])));
        assert!(registry
            .begin_channel_load(&channel, "room", false)
            .is_some());
    }

    #[test]
    fn failed_channel_provider_refresh_preserves_only_that_providers_set_and_allows_retry() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("room");
        load(
            &mut registry,
            &channel,
            vec![
                emote("ffz", "keep", "Keep"),
                emote("7tv", "remove", "Remove"),
            ],
        );
        let generation = registry.begin_channel_load(&channel, "room", true).unwrap();
        assert!(registry.replace_channel(&channel, generation, "ffz", None));
        assert!(registry
            .begin_channel_load(&channel, "room", false)
            .is_none());
        assert!(registry.replace_channel(&channel, generation, "7tv", Some(vec![])));
        assert_eq!(registry.resolve(&channel, "", "Keep").unwrap().id, "keep");
        assert!(registry.resolve(&channel, "", "Remove").is_none());
        assert_eq!(registry.channel_emote_count(&channel), 1);
        assert!(registry.complete_channel_load(&channel, generation));
        assert!(registry.begin_channel_load(&channel, "room", false).is_none());
        let next = registry
            .take_channel_emote_retries(Instant::now() + CHANNEL_RETRY_DELAY, 1)
            .pop()
            .unwrap()
            .generation;
        assert_ne!(generation, next);
        assert!(!registry.replace_channel(&channel, generation, "ffz", Some(vec![])));
    }

    #[test]
    fn failed_channel_providers_retry_after_cooldown_and_preserve_successful_sets() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("room");
        let now = Instant::now();
        registry.open_channel(&channel);
        let initial = registry.begin_channel_load(&channel, "42", false).unwrap();
        registry.replace_channel(
            &channel,
            initial,
            "bttv",
            Some(vec![emote("bttv", "kept", "Keep")]),
        );
        registry.replace_channel(&channel, initial, "7tv", Some(Vec::new()));
        registry.replace_channel(&channel, initial, "ffz", None);
        assert!(registry.complete_channel_load_at(&channel, initial, now));
        // Repeated room-state updates must not reload successful providers or
        // bypass the failed provider's cooldown.
        for _ in 0..3 {
            assert!(registry.begin_channel_load(&channel, "42", false).is_none());
        }
        assert!(registry
            .take_channel_emote_retries(now + CHANNEL_RETRY_DELAY - Duration::from_millis(1), 4)
            .is_empty());

        let mut retries = registry.take_channel_emote_retries(now + CHANNEL_RETRY_DELAY, 4);
        assert_eq!(retries.len(), 1);
        let retry = retries.pop().unwrap();
        assert_eq!(retry.channel, channel);
        assert_eq!(retry.room_id, "42");
        assert_eq!(retry.providers, ["ffz"]);
        assert_ne!(retry.generation, initial);
        assert!(registry
            .take_channel_emote_retries(now + CHANNEL_RETRY_DELAY, 4)
            .is_empty());
        assert_eq!(registry.resolve(&channel, "", "Keep").unwrap().id, "kept");

        registry.replace_channel(
            &channel,
            retry.generation,
            "ffz",
            Some(vec![emote("ffz", "new", "Recovered")]),
        );
        registry.complete_channel_load_at(&channel, retry.generation, now + CHANNEL_RETRY_DELAY);
        assert_eq!(
            registry.resolve(&channel, "", "Recovered").unwrap().id,
            "new"
        );
        assert_eq!(registry.resolve(&channel, "", "Keep").unwrap().id, "kept");
        assert!(registry
            .take_channel_emote_retries(now + CHANNEL_RETRY_DELAY * 10, 4)
            .is_empty());
    }

    #[test]
    fn retries_drop_recovered_providers_and_accept_an_empty_inventory() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("room");
        let now = Instant::now();
        registry.open_channel(&channel);
        let initial = registry.begin_channel_load(&channel, "42", false).unwrap();
        for provider in ["7tv", "ffz"] {
            registry.replace_channel(&channel, initial, provider, None);
        }
        registry.complete_channel_load_at(&channel, initial, now);
        let retry = registry
            .take_channel_emote_retries(now + CHANNEL_RETRY_DELAY, 4)
            .pop()
            .unwrap();
        assert_eq!(retry.providers, ["7tv", "ffz"]);
        registry.replace_channel(&channel, retry.generation, "ffz", Some(Vec::new()));
        registry.replace_channel(&channel, retry.generation, "7tv", None);
        registry.complete_channel_load_at(&channel, retry.generation, now + CHANNEL_RETRY_DELAY);
        let retry = registry
            .take_channel_emote_retries(now + CHANNEL_RETRY_DELAY * 2, 4)
            .pop()
            .unwrap();
        assert_eq!(retry.providers, ["7tv"]);
        registry.replace_channel(&channel, retry.generation, "7tv", Some(Vec::new()));
        registry.complete_channel_load_at(
            &channel,
            retry.generation,
            now + CHANNEL_RETRY_DELAY * 2,
        );
        assert!(registry
            .take_channel_emote_retries(now + CHANNEL_RETRY_DELAY * 3, 4)
            .is_empty());
    }

    #[test]
    fn retries_share_the_channel_load_budget_and_public_scope_survives_account_changes() {
        let mut registry = EmoteRegistry::default();
        let now = Instant::now();
        for index in 0..6 {
            let channel = ChannelId::new(&format!("room{index}"));
            registry.open_channel(&channel);
            let initial = registry.begin_channel_load(&channel, "42", false).unwrap();
            registry.replace_channel(&channel, initial, "ffz", None);
            registry.complete_channel_load_at(&channel, initial, now);
        }
        let loading = ChannelId::new("new-room");
        registry.open_channel(&loading);
        registry.begin_channel_load(&loading, "43", false).unwrap();
        registry.set_account(Some("account-a"));
        registry.set_account(Some("account-b"));
        let retries = registry.take_channel_emote_retries(now + CHANNEL_RETRY_DELAY, 4);
        assert_eq!(retries.len(), 3);
        assert!(registry
            .take_channel_emote_retries(now + CHANNEL_RETRY_DELAY, 4)
            .is_empty());
        registry.close_channel(&retries[0].channel);
        let next = registry.take_channel_emote_retries(now + CHANNEL_RETRY_DELAY, 4);
        assert_eq!(next.len(), 1);
        assert!(!retries.iter().any(|retry| retry.channel == next[0].channel));
        assert!(!registry.complete_channel_load(&retries[0].channel, retries[0].generation));
    }

    #[test]
    fn manual_reload_reconnect_and_departure_cancel_old_channel_retries() {
        let mut registry = EmoteRegistry::default();
        let now = Instant::now();
        let channels: Vec<_> = ["manual", "reconnect", "depart", "replacement"]
            .into_iter()
            .map(ChannelId::new)
            .collect();
        let mut generations = Vec::new();
        for channel in &channels {
            registry.open_channel(channel);
            let generation = registry.begin_channel_load(channel, "42", false).unwrap();
            registry.replace_channel(channel, generation, "ffz", None);
            registry.complete_channel_load_at(channel, generation, now);
            generations.push(generation);
        }
        let manual = registry
            .begin_channel_load(&channels[0], "42", true)
            .unwrap();
        registry.replace_channel(&channels[0], manual, "ffz", Some(Vec::new()));
        registry.complete_channel_load_at(&channels[0], manual, now);
        registry.reconnect_channel(&channels[1]);
        registry.close_channel(&channels[2]);
        registry.open_channel(&channels[2]);
        registry
            .begin_channel_load(&channels[3], "84", false)
            .unwrap();
        for (channel, generation) in channels.iter().zip(generations) {
            assert!(!registry.replace_channel(
                channel,
                generation,
                "ffz",
                Some(vec![emote("ffz", "old", "Obsolete")])
            ));
        }
        assert!(registry
            .take_channel_emote_retries(now + CHANNEL_RETRY_DELAY * 2, 4)
            .is_empty());
    }

    #[test]
    fn deferred_history_users_resume_after_capacity_frees_and_the_active_account_goes_first() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("room");
        registry.open_channel(&channel);
        registry.set_account(Some("account"));
        let mut first = 0;
        for user in 0..MAX_PENDING_USERS {
            let generation = registry
                .begin_personal_load(&user.to_string(), Some(&channel))
                .unwrap();
            if user == 0 {
                first = generation;
            }
        }
        assert!(registry
            .begin_personal_load("history", Some(&channel))
            .is_none());
        registry.defer_personal_load("history", Some(&channel));
        registry.defer_personal_load("history", Some(&channel));
        registry.defer_personal_load("account", None);
        assert_eq!(registry.deferred_users.len(), 2);
        registry
            .complete_personal_load("0", first, Some(vec![]))
            .unwrap();
        let (account, owner) = registry.next_deferred_personal_load().unwrap();
        assert_eq!(account, "account");
        assert!(owner.is_none());
        let generation = registry
            .begin_personal_load(&account, owner.as_ref())
            .unwrap();
        registry
            .complete_personal_load(&account, generation, Some(vec![]))
            .unwrap();
        let (history, owner) = registry.next_deferred_personal_load().unwrap();
        assert_eq!(history, "history");
        assert!(registry
            .begin_personal_load(&history, owner.as_ref())
            .is_some());
    }

    #[test]
    fn deferred_user_retention_is_bounded_and_departed_channels_are_discarded() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("room");
        registry.open_channel(&channel);
        registry.set_account(Some("account"));
        for user in 0..MAX_CACHED_USERS + 10 {
            registry.defer_personal_load(&user.to_string(), Some(&channel));
        }
        registry.defer_personal_load("account", None);
        assert_eq!(registry.deferred_users.len(), MAX_CACHED_USERS);
        assert_eq!(registry.deferred_user_order.len(), MAX_CACHED_USERS);
        registry.close_channel(&channel);
        assert_eq!(registry.deferred_users.len(), 1);
        assert_eq!(
            registry.next_deferred_personal_load(),
            Some(("account".into(), None))
        );
        assert!(registry.next_deferred_personal_load().is_none());
    }

    #[test]
    fn personal_emotes_only_apply_to_the_owner_and_stale_account_loads_are_rejected() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("room");
        registry.set_account(Some("alice-id"));
        let generation = registry.begin_personal_load("alice-id", None).unwrap();
        assert!(registry
            .complete_personal_load(
                "alice-id",
                generation,
                Some(vec![emote("7tv", "personal", "Mine")])
            )
            .is_some());
        assert!(registry.resolve(&channel, "alice-id", "Mine").is_some());
        assert!(registry.resolve(&channel, "bob-id", "Mine").is_none());
        let kick = ChannelId::kick("room");
        assert!(registry.resolve(&kick, "alice-id", "Mine").is_none());
        assert!(!registry.catalog()[0].is_available_for(&kick, "alice-id"));
        registry.cached_users.remove("alice-id");
        let pending = registry.begin_personal_load("alice-id", None).unwrap();
        registry.set_account(None);
        registry.set_account(Some("alice-id"));
        assert!(registry
            .complete_personal_load("alice-id", pending, Some(vec![emote("7tv", "old", "Mine")]))
            .is_none());
        assert_eq!(
            registry.resolve(&channel, "alice-id", "Mine").unwrap().id,
            "personal"
        );
    }

    #[test]
    fn whisper_sender_lookups_are_owned_bounded_and_cleared_when_accounts_change() {
        let mut registry = EmoteRegistry::default();
        assert!(!registry.queue_whisper_sender("account", "sender"));
        registry.set_account(Some("account"));
        assert!(!registry.queue_whisper_sender("other-account", "sender"));
        assert!(!registry.queue_whisper_sender("account", ""));
        assert!(registry.queue_whisper_sender("account", "sender"));
        assert!(registry.queue_whisper_sender("account", "sender"));
        assert_eq!(registry.deferred_users.len(), 1);
        let (sender, channel) = registry.next_deferred_personal_load().unwrap();
        assert_eq!(sender, "sender");
        assert!(channel.is_none());
        let pending = registry.begin_personal_load(&sender, None).unwrap();
        for user in 0..MAX_CACHED_USERS {
            registry.queue_whisper_sender("account", &user.to_string());
        }
        assert_eq!(registry.whisper_senders.len(), MAX_CACHED_USERS);
        assert!(registry.deferred_users.len() <= MAX_CACHED_USERS);
        assert!(!registry.queue_whisper_sender("account", "overflow"));
        registry.set_account(Some("next-account"));
        assert!(registry.whisper_senders.is_empty());
        assert!(registry.next_deferred_personal_load().is_none());
        assert!(registry
            .complete_personal_load(&sender, pending, Some(vec![]))
            .is_none());
        assert!(!registry.queue_whisper_sender("account", "old-replay"));
        assert!(registry.queue_whisper_sender("next-account", "sender"));
    }

    #[test]
    fn sender_lookups_are_deduplicated_bounded_and_rejected_after_leaving() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("room");
        registry.open_channel(&channel);
        let first = registry.begin_personal_load("0", Some(&channel)).unwrap();
        assert!(registry.begin_personal_load("0", Some(&channel)).is_none());
        for user in 1..MAX_PENDING_USERS {
            assert!(registry
                .begin_personal_load(&user.to_string(), Some(&channel))
                .is_some());
        }
        assert!(registry
            .begin_personal_load("overflow", Some(&channel))
            .is_none());
        registry.close_channel(&channel);
        registry.open_channel(&channel);
        assert!(registry
            .complete_personal_load("0", first, Some(vec![]))
            .is_none());
        let current = registry.begin_personal_load("0", Some(&channel)).unwrap();
        assert!(registry
            .complete_personal_load("0", first, Some(vec![]))
            .is_none());
        assert!(registry
            .complete_personal_load("0", current, Some(vec![emote("7tv", "current", "Hello")]))
            .is_some());
        assert!(registry.resolve(&channel, "0", "Hello").is_some());
        assert!(registry.resolve(&channel, "1", "Hello").is_none());
    }

    #[test]
    fn negative_results_are_cached_and_sender_cache_retention_is_bounded() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("room");
        registry.open_channel(&channel);
        for user in 0..=MAX_CACHED_USERS {
            let user = user.to_string();
            let generation = registry.begin_personal_load(&user, Some(&channel)).unwrap();
            let update = registry
                .complete_personal_load(&user, generation, Some(vec![]))
                .unwrap();
            assert!(!update.catalog_changed);
            assert!(registry
                .begin_personal_load(&user, Some(&channel))
                .is_none());
            if user == MAX_CACHED_USERS.to_string() {
                assert_eq!(update.evicted_users, ["0"]);
            }
        }
        assert_eq!(registry.cached_users.len(), MAX_CACHED_USERS);
        assert_eq!(registry.cached_user_order.len(), MAX_CACHED_USERS);
        assert!(registry.begin_personal_load("0", Some(&channel)).is_some());
    }

    #[test]
    fn personal_sets_have_per_sender_and_total_bounds() {
        let mut registry = EmoteRegistry::default();
        let channel = ChannelId::new("room");
        registry.open_channel(&channel);
        let sets = MAX_PERSONAL_EMOTES / MAX_PERSONAL_EMOTES_PER_USER;
        for user in 0..=sets {
            let user = user.to_string();
            let generation = registry.begin_personal_load(&user, Some(&channel)).unwrap();
            let emotes = (0..=MAX_PERSONAL_EMOTES_PER_USER)
                .map(|code| emote("7tv", &format!("{user}-{code}"), &format!("Code{code}")))
                .collect();
            registry
                .complete_personal_load(&user, generation, Some(emotes))
                .unwrap();
        }
        assert_eq!(registry.personal_emote_count, MAX_PERSONAL_EMOTES);
        assert!(registry.resolve(&channel, "0", "Code0").is_none());
        assert!(registry
            .resolve(&channel, &sets.to_string(), "Code0")
            .is_some());
        assert!(registry
            .resolve(
                &channel,
                &sets.to_string(),
                &format!("Code{MAX_PERSONAL_EMOTES_PER_USER}")
            )
            .is_none());
    }

    #[test]
    fn failed_profile_refresh_keeps_the_last_personal_set_and_backs_off() {
        let mut registry = EmoteRegistry::default();
        registry.set_account(Some("42"));
        let generation = registry.begin_personal_load("42", None).unwrap();
        registry
            .complete_personal_load("42", generation, Some(vec![emote("7tv", "old", "Same")]))
            .unwrap();
        registry.cached_users.remove("42");
        let retry = registry.begin_personal_load("42", None).unwrap();
        let update = registry.complete_personal_load("42", retry, None).unwrap();
        assert!(!update.catalog_changed);
        assert!(registry.begin_personal_load("42", None).is_none());
        assert_eq!(
            registry
                .resolve(&ChannelId::new("room"), "42", "Same")
                .unwrap()
                .id,
            "old"
        );
    }

    #[test]
    fn native_twitch_offsets_take_priority_over_personal_emotes() {
        let mut registry = EmoteRegistry::default();
        registry.set_account(Some("42"));
        let generation = registry.begin_personal_load("42", None).unwrap();
        registry
            .complete_personal_load(
                "42",
                generation,
                Some(vec![emote("7tv", "personal", "Same")]),
            )
            .unwrap();
        let channel = ChannelId::new("room");
        let spans = crust_core::format::tokenize(
            "Same",
            false,
            &[crust_core::TwitchEmotePos {
                id: "native".into(),
                start: 0,
                end: 3,
            }],
            &|code| {
                registry.resolve(&channel, "42", code).map(|info| {
                    (
                        info.id.clone(),
                        info.code.clone(),
                        info.url_1x.clone(),
                        info.provider.clone(),
                        None,
                    )
                })
            },
        );
        assert!(
            matches!(&spans[0], crust_core::Span::Emote { id, provider, .. }
            if id == "native" && provider == "twitch")
        );
    }
}

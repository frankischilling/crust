use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crust_core::model::{ChannelId, EmoteCatalogEntry, UserId};
use crust_emotes::EmoteInfo;

type ProviderEmotes = HashMap<String, HashMap<String, EmoteInfo>>;

const MAX_PENDING_USERS: usize = 64;
const MAX_CACHED_USERS: usize = 2_048;
const MAX_PERSONAL_EMOTES_PER_USER: usize = 256;
const MAX_PERSONAL_EMOTES: usize = 16_384;
const USER_LOOKUP_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone)]
struct UserLookup {
    generation: u64,
    channel: Option<ChannelId>,
}

pub(crate) struct UserEmoteUpdate {
    pub(crate) evicted_users: Vec<String>,
    pub(crate) catalog_changed: bool,
}

#[derive(Clone, Default)]
struct ChannelEmotes {
    generation: u64,
    source_id: Option<String>,
    emotes: ProviderEmotes,
    /// Definitions carried by incoming messages survive older fetched snapshots.
    inline_emotes: ProviderEmotes,
}

/// Emote definitions remain scoped to the channel or sender that owns them.
#[derive(Clone, Default)]
pub(crate) struct EmoteRegistry {
    global: ProviderEmotes,
    channels: HashMap<ChannelId, ChannelEmotes>,
    personal: HashMap<String, ProviderEmotes>,
    pending_users: HashMap<String, UserLookup>,
    deferred_users: HashMap<String, Option<ChannelId>>,
    deferred_user_order: VecDeque<String>,
    cached_users: HashMap<String, Instant>,
    cached_user_order: VecDeque<String>,
    personal_emote_count: usize,
    active_account: Option<String>,
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

impl EmoteRegistry {
    pub(crate) fn replace_global(&mut self, provider: &str, emotes: Vec<EmoteInfo>) {
        self.global.insert(
            provider.to_owned(),
            emotes
                .into_iter()
                .map(|emote| (emote.code.clone(), emote))
                .collect(),
        );
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
    }

    /// Reserve a generation before spawning a load, rejecting mode-only refreshes.
    pub(crate) fn begin_channel_load(
        &mut self,
        channel: &ChannelId,
        source_id: &str,
        force: bool,
    ) -> Option<u64> {
        let scope = self.channels.get_mut(channel)?;
        if !force && scope.source_id.as_deref() == Some(source_id) {
            return None;
        }
        self.next_generation = self.next_generation.wrapping_add(1);
        scope.generation = self.next_generation;
        scope.source_id = Some(source_id.to_owned());
        Some(scope.generation)
    }

    pub(crate) fn replace_channel(
        &mut self,
        channel: &ChannelId,
        generation: u64,
        provider: &str,
        emotes: Option<Vec<EmoteInfo>>,
    ) -> bool {
        let Some(scope) = self.channels.get_mut(channel) else {
            return false;
        };
        if scope.generation != generation {
            return false;
        }
        if let Some(emotes) = emotes {
            scope.emotes.insert(
                provider.to_owned(),
                emotes
                    .into_iter()
                    .map(|emote| (emote.code.clone(), emote))
                    .collect(),
            );
        } else {
            // Keep the last usable set and let a later room state retry this load.
            scope.source_id = None;
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
        self.pending_users.clear();
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
                None => self.active_account.as_deref() == Some(user_id),
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

    /// Reserve one bounded lookup for a sender observed in an open channel or the active account.
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
            None if self.active_account.as_deref() == Some(user_id) => None,
            _ => return None,
        };
        self.next_generation = self.next_generation.wrapping_add(1);
        self.pending_users.insert(
            user_id.to_owned(),
            UserLookup {
                generation: self.next_generation,
                channel,
            },
        );
        Some(self.next_generation)
    }

    pub(crate) fn complete_personal_load(
        &mut self,
        user_id: &str,
        generation: u64,
        emotes: Option<Vec<EmoteInfo>>,
    ) -> Option<UserEmoteUpdate> {
        if self
            .pending_users
            .get(user_id)
            .map(|lookup| lookup.generation)
            != Some(generation)
        {
            return None;
        }
        self.pending_users.remove(user_id);
        let retry_after = if emotes.is_some() {
            USER_LOOKUP_TTL
        } else {
            Duration::from_secs(30)
        };
        let mut catalog_changed = false;
        if let Some(mut emotes) = emotes {
            emotes.truncate(MAX_PERSONAL_EMOTES_PER_USER);
            catalog_changed = !emotes.is_empty();
            if let Some(old) = self.personal.remove(user_id) {
                self.personal_emote_count -= old.values().map(HashMap::len).sum::<usize>();
                catalog_changed = true;
            }
            if !emotes.is_empty() {
                let providers = provider_maps(emotes);
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
                .map(|scope| HashMap::from([(channel.clone(), scope.clone())]))
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
        }
        for (user_id, emotes) in &self.personal {
            append(emotes, None, Some(user_id), None);
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
        let third = registry.begin_channel_load(&channel, "room", true).unwrap();
        assert!(registry.replace_channel(&channel, third, "ffz", Some(vec![])));
        assert!(registry.resolve(&channel, "", "Same").is_none());
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
        assert!(registry.replace_channel(&channel, generation, "7tv", Some(vec![])));
        assert_eq!(registry.resolve(&channel, "", "Keep").unwrap().id, "keep");
        assert!(registry.resolve(&channel, "", "Remove").is_none());
        assert_eq!(registry.channel_emote_count(&channel), 1);
        let next = registry
            .begin_channel_load(&channel, "room", false)
            .unwrap();
        assert_ne!(generation, next);
        assert!(!registry.replace_channel(&channel, generation, "ffz", Some(vec![])));
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

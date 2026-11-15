use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crust_emotes::EmoteInfo;

use super::{EmoteDelta, LiveBinding, LiveSource};

static NEXT_BINDING: AtomicU64 = AtomicU64::new(1);

const MAX_JOURNAL: usize = 4_096;
const MAX_RETIRED_SETS: usize = 64;
// The public cached endpoint currently advertises five minutes of freshness
// plus ten minutes of stale-while-revalidate. Keep a wider reconciliation window.
const CACHED_SNAPSHOT_WINDOW: Duration = Duration::from_secs(30 * 60);

pub(super) type Emotes = HashMap<String, EmoteInfo>;

#[derive(Clone)]
struct ObservedDelta {
    revision: u64,
    observed_at: Instant,
    delta: EmoteDelta,
}

#[derive(Clone)]
pub(crate) struct LiveState {
    pub(crate) source: LiveSource,
    pub(crate) binding_generation: u64,
    epoch: u64,
    revision: u64,
    journal: VecDeque<ObservedDelta>,
    overflowed_at: Option<(Instant, u64)>,
    fresh_snapshot_at: Option<Instant>,
    retired_sets: VecDeque<(Option<String>, Instant)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotTicket {
    pub(crate) epoch: u64,
    revision: u64,
    started_at: Instant,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ApplyResult {
    pub(crate) changed: bool,
    pub(crate) needs_snapshot: bool,
}

impl LiveState {
    pub(crate) fn new(source: LiveSource) -> Self {
        Self {
            source,
            binding_generation: NEXT_BINDING.fetch_add(1, Ordering::Relaxed),
            epoch: 0,
            revision: 0,
            journal: VecDeque::new(),
            overflowed_at: None,
            fresh_snapshot_at: None,
            retired_sets: VecDeque::new(),
        }
    }

    pub(crate) fn binding(&self) -> LiveBinding {
        LiveBinding {
            generation: self.binding_generation,
            source: self.source.clone(),
        }
    }

    pub(crate) fn rebind(&mut self, source: LiveSource) {
        if self.source == source {
            return;
        }
        if self.source.same_catalog(&source) {
            self.source = source;
            self.binding_generation = NEXT_BINDING.fetch_add(1, Ordering::Relaxed);
            self.epoch = 0;
        } else {
            let retired_sets = std::mem::take(&mut self.retired_sets);
            *self = Self::new(source);
            self.retired_sets = retired_sets;
        }
    }

    pub(crate) fn rebind_authoritative(&mut self, source: LiveSource, now: Instant) {
        if self.source.same_catalog(&source) {
            self.rebind(source);
            return;
        }
        let previous = self.source.set_id.clone();
        self.rebind(source);
        self.retired_sets.retain(|(set, retired)| {
            set != &self.source.set_id
                && set != &previous
                && now.saturating_duration_since(*retired) < CACHED_SNAPSHOT_WINDOW
        });
        if self.retired_sets.len() >= MAX_RETIRED_SETS {
            self.retired_sets.pop_front();
        }
        self.retired_sets.push_back((previous, now));
    }

    pub(crate) fn accepts_cached_source(&self, source: &LiveSource) -> bool {
        // A request started after a live switch can still return the previous
        // assignment from an edge cache. Keep those source removals bounded.
        !self.retired_sets.iter().any(|(set, retired)| {
            set == &source.set_id && retired.elapsed() < CACHED_SNAPSHOT_WINDOW
        })
    }

    pub(crate) fn begin_epoch(&mut self, epoch: u64) -> bool {
        if epoch <= self.epoch {
            return false;
        }
        self.epoch = epoch;
        true
    }

    pub(crate) fn ticket(&self, epoch: u64) -> Option<SnapshotTicket> {
        (epoch == self.epoch).then_some(SnapshotTicket {
            epoch,
            revision: self.revision,
            started_at: Instant::now(),
        })
    }

    pub(crate) fn observe(
        &mut self,
        emotes: &mut Emotes,
        epoch: u64,
        delta: EmoteDelta,
        now: Instant,
    ) -> ApplyResult {
        if epoch != self.epoch {
            return ApplyResult::default();
        }
        self.revision = self.revision.wrapping_add(1);
        // Duplicate deliveries still advance the fence: a second removal can
        // arrive after a snapshot started even when its payload is unchanged.
        if let Some(last) = self.journal.back_mut().filter(|last| last.delta == delta) {
            last.revision = self.revision;
            last.observed_at = now;
            return ApplyResult {
                changed: apply_delta(emotes, &delta),
                needs_snapshot: false,
            };
        }
        if self.journal.len() >= MAX_JOURNAL || self.overflowed_at.is_some() {
            self.overflowed_at = Some((now, self.revision));
            return ApplyResult {
                changed: false,
                needs_snapshot: true,
            };
        }
        let changed = apply_delta(emotes, &delta);
        self.journal.push_back(ObservedDelta {
            revision: self.revision,
            observed_at: now,
            delta,
        });
        ApplyResult {
            changed,
            needs_snapshot: false,
        }
    }

    pub(crate) fn replace_snapshot(
        &mut self,
        current: &mut Emotes,
        ticket: SnapshotTicket,
        snapshot: Vec<EmoteInfo>,
        fresh: bool,
        now: Instant,
    ) -> ApplyResult {
        if ticket.epoch != self.epoch {
            return ApplyResult::default();
        }
        if self
            .overflowed_at
            .is_some_and(|(_, revision)| revision > ticket.revision)
        {
            return ApplyResult {
                changed: false,
                needs_snapshot: true,
            };
        }
        // A fresh authoritative read includes changes missed while disconnected.
        // Cached reads retire earlier deltas after their conservative stale window.
        // Changes received after this request started must always be replayed.
        while self.journal.front().is_some_and(|entry| {
            entry.revision <= ticket.revision
                && (fresh
                    || ticket
                        .started_at
                        .saturating_duration_since(entry.observed_at)
                        >= CACHED_SNAPSHOT_WINDOW)
        }) {
            self.journal.pop_front();
        }
        if self.overflowed_at.is_some_and(|(when, _)| {
            fresh || ticket.started_at.saturating_duration_since(when) >= CACHED_SNAPSHOT_WINDOW
        }) {
            self.overflowed_at = None;
        }
        let mut next: Emotes = snapshot
            .into_iter()
            .map(|emote| (emote.code.clone(), emote))
            .collect();
        for entry in &self.journal {
            apply_delta(&mut next, &entry.delta);
        }
        let changed = *current != next;
        *current = next;
        if fresh {
            self.fresh_snapshot_at = Some(now);
        }
        ApplyResult {
            changed,
            needs_snapshot: self.overflowed_at.is_some(),
        }
    }

    /// Initial/manual REST loads obey the same tombstones as reconnect snapshots.
    pub(crate) fn overlay_snapshot(
        &self,
        snapshot: Vec<EmoteInfo>,
        current: Option<&Emotes>,
    ) -> Emotes {
        // Cached metadata must not replace a recent authoritative snapshot after
        // that snapshot has retired the older live-change journal.
        if self
            .fresh_snapshot_at
            .is_some_and(|fresh| fresh.elapsed() < CACHED_SNAPSHOT_WINDOW)
        {
            return current.cloned().unwrap_or_default();
        }
        let mut next = snapshot
            .into_iter()
            .map(|emote| (emote.code.clone(), emote))
            .collect();
        for entry in &self.journal {
            apply_delta(&mut next, &entry.delta);
        }
        next
    }
}

pub(super) fn apply_delta(emotes: &mut Emotes, delta: &EmoteDelta) -> bool {
    match delta {
        EmoteDelta::Upsert { emote, old_code, preserve_overlay } => {
            let retained_overlay = preserve_overlay.then(|| {
                old_code.as_ref().and_then(|code| emotes.get(code))
                    .or_else(|| emotes.get(&emote.code))
                    .filter(|old| old.id == emote.id && old.zero_width != emote.zero_width)
                    .map(|old| {
                        let mut updated = emote.clone();
                        updated.zero_width = old.zero_width;
                        updated
                    })
            }).flatten();
            let emote = retained_overlay.as_ref().unwrap_or(emote);
            let mut changed = false;
            if let Some(old_code) = old_code.as_ref().filter(|old| *old != &emote.code) {
                if emotes.get(old_code).is_some_and(|old| old.id == emote.id) {
                    emotes.remove(old_code);
                    changed = true;
                }
            }
            if emotes.get(&emote.code) != Some(emote) {
                emotes.insert(emote.code.clone(), emote.clone());
                changed = true;
            }
            changed
        }
        EmoteDelta::Rename { id, old_code, code } => {
            let old = old_code
                .as_ref()
                .and_then(|code| emotes.get(code))
                .filter(|emote| emote.id == *id)
                .or_else(|| {
                    old_code
                        .is_none()
                        .then(|| emotes.values().find(|emote| emote.id == *id))
                        .flatten()
                })
                .cloned();
            let Some(mut emote) = old else {
                return false;
            };
            if emote.code == *code {
                return false;
            }
            emotes.remove(&emote.code);
            emote.code = code.clone();
            emotes.insert(code.clone(), emote);
            true
        }
        EmoteDelta::Overlay { id, code, zero_width } => {
            // Flags belong to an active alias. An ID alone is only unambiguous
            // when the set contains a single alias for that emote.
            if code.is_none() && emotes.values().filter(|emote| emote.id == *id).take(2).count() != 1 {
                return false;
            }
            let mut changed = false;
            for emote in emotes.values_mut().filter(|emote| {
                emote.id == *id && code.as_ref().is_none_or(|code| emote.code == *code)
            }) {
                if emote.zero_width != *zero_width {
                    emote.zero_width = *zero_width;
                    changed = true;
                }
            }
            changed
        }
        EmoteDelta::Remove { id, code } => match code {
            Some(code) if emotes.get(code).is_some_and(|emote| emote.id == *id) => {
                emotes.remove(code).is_some()
            }
            Some(_) => false,
            None => {
                let before = emotes.len();
                emotes.retain(|_, emote| emote.id != *id);
                before != emotes.len()
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::live_emotes::Provider;

    fn emote(id: &str, code: &str) -> EmoteInfo {
        EmoteInfo {
            id: id.into(),
            code: code.into(),
            provider: "bttv".into(),
            zero_width: false,
            url_1x: format!("https://example.test/{id}/1x"),
            url_2x: None,
            url_4x: None,
        }
    }

    #[test]
    fn overlay_changes_preserve_assets_and_do_not_change_other_aliases() {
        let mut emotes = Emotes::from([
            ("Base".into(), emote("same", "Base")),
            ("Alias".into(), emote("same", "Alias")),
            ("Other".into(), emote("different", "Other")),
        ]);
        let delta = EmoteDelta::Overlay {
            id: "same".into(), code: Some("Alias".into()), zero_width: true,
        };
        assert!(!apply_delta(&mut emotes, &EmoteDelta::Overlay {
            id: "same".into(), code: None, zero_width: true,
        }));
        assert!(apply_delta(&mut emotes, &delta));
        assert!(!apply_delta(&mut emotes, &delta));
        assert!(emotes["Alias"].zero_width);
        assert!(!emotes["Base"].zero_width);
        assert!(!emotes["Other"].zero_width);
        assert_eq!(emotes["Alias"].url_1x, "https://example.test/same/1x");
        let mut replacement = emote("same", "Alias");
        replacement.url_1x = "https://example.test/replacement.webp".into();
        assert!(apply_delta(&mut emotes, &EmoteDelta::Upsert {
            emote: replacement, old_code: None, preserve_overlay: true,
        }));
        assert!(emotes["Alias"].zero_width);
        assert_eq!(emotes["Alias"].url_1x, "https://example.test/replacement.webp");
        assert!(apply_delta(&mut emotes, &EmoteDelta::Rename {
            id: "same".into(), old_code: Some("Alias".into()), code: "Renamed".into(),
        }));
        assert!(emotes["Renamed"].zero_width);
        assert!(!apply_delta(&mut emotes, &delta));
        assert!(apply_delta(&mut emotes, &EmoteDelta::Overlay {
            id: "same".into(), code: Some("Renamed".into()), zero_width: false,
        }));
        assert!(!emotes["Renamed"].zero_width);
    }

    fn state() -> LiveState {
        LiveState::new(LiveSource {
            provider: Provider::Bttv,
            room_id: "123".into(),
            platform: "TWITCH".into(),
            user_id: None,
            set_id: None,
            connection_index: None,
        })
    }

    #[test]
    fn a_cached_snapshot_cannot_resurrect_a_removed_emote_after_reconnect() {
        let now = Instant::now();
        let old = emote("id", "Gone");
        let mut state = state();
        let mut current = HashMap::from([("Gone".into(), old.clone())]);
        assert!(state.begin_epoch(1));
        let before_delete = state.ticket(1).unwrap();
        assert!(
            state
                .observe(
                    &mut current,
                    1,
                    EmoteDelta::Remove {
                        id: "id".into(),
                        code: None
                    },
                    now
                )
                .changed
        );
        assert!(
            !state
                .replace_snapshot(&mut current, before_delete, vec![old.clone()], false, now)
                .changed
        );
        assert!(current.is_empty());
        assert!(state.begin_epoch(2));
        assert!(
            !state
                .replace_snapshot(&mut current, before_delete, vec![old.clone()], false, now)
                .changed
        );
        let reconnect = state.ticket(2).unwrap();
        assert!(
            !state
                .replace_snapshot(
                    &mut current,
                    reconnect,
                    vec![old],
                    false,
                    now + Duration::from_secs(20)
                )
                .changed
        );
        assert!(current.is_empty());
        assert_eq!(state.journal.len(), 1);
    }

    #[test]
    fn a_recent_fresh_snapshot_prevents_cached_refresh_from_restoring_removed_emotes() {
        let now = Instant::now();
        let old = emote("id", "Gone");
        let mut state = state();
        let mut current = HashMap::from([("Gone".into(), old.clone())]);
        state.begin_epoch(1);
        state.observe(
            &mut current,
            1,
            EmoteDelta::Remove {
                id: "id".into(),
                code: None,
            },
            now,
        );
        let confirmed = state.ticket(1).unwrap();
        state.replace_snapshot(&mut current, confirmed, Vec::new(), true, now);
        assert!(state.journal.is_empty());
        assert!(state
            .overlay_snapshot(vec![old.clone()], Some(&current))
            .is_empty());

        let mut replacement = state.source.clone();
        replacement.set_id = Some("new-set".into());
        state.rebind(replacement);
        assert!(state
            .overlay_snapshot(vec![old], Some(&current))
            .contains_key("Gone"));
    }

    #[test]
    fn retired_sources_expire_and_authoritative_changes_can_restore_a_previous_set() {
        let now = Instant::now();
        let mut state = state();
        let no_set = state.source.clone();
        let mut first = no_set.clone();
        first.set_id = Some("first".into());
        state.rebind_authoritative(first.clone(), now);
        assert!(!state.accepts_cached_source(&no_set));
        let mut second = first.clone();
        second.set_id = Some("second".into());
        state.rebind_authoritative(second.clone(), now);
        assert!(!state.accepts_cached_source(&first));
        state.rebind_authoritative(first.clone(), now);
        assert!(state.accepts_cached_source(&first));
        assert!(!state.accepts_cached_source(&second));

        let mut expired = LiveState::new(no_set.clone());
        expired.rebind_authoritative(first, now - CACHED_SNAPSHOT_WINDOW - Duration::from_secs(1));
        assert!(expired.accepts_cached_source(&no_set));
    }

    #[test]
    fn cached_refresh_can_recover_when_authoritative_snapshots_stop() {
        let old = emote("id", "Back");
        let mut state = state();
        let mut current = HashMap::new();
        state.begin_epoch(1);
        let confirmed = state.ticket(1).unwrap();
        state.replace_snapshot(
            &mut current,
            confirmed,
            Vec::new(),
            true,
            Instant::now() - CACHED_SNAPSHOT_WINDOW - Duration::from_secs(1),
        );
        assert!(state
            .overlay_snapshot(vec![old], Some(&current))
            .contains_key("Back"));
    }

    #[test]
    fn snapshot_reconciliation_preserves_newer_updates_and_partial_rename_urls() {
        let now = Instant::now();
        let original = emote("id", "Old");
        let mut state = state();
        let mut current = HashMap::from([("Old".into(), original.clone())]);
        state.begin_epoch(1);
        let ticket = state.ticket(1).unwrap();
        state.observe(
            &mut current,
            1,
            EmoteDelta::Rename {
                id: "id".into(),
                old_code: None,
                code: "New".into(),
            },
            now,
        );
        state.replace_snapshot(&mut current, ticket, vec![original.clone()], true, now);
        assert!(!current.contains_key("Old"));
        assert_eq!(current["New"].url_1x, original.url_1x);
        let confirmed = state.ticket(1).unwrap();
        state.replace_snapshot(&mut current, confirmed, vec![emote("id", "New")], true, now);
        assert!(state.journal.is_empty());
    }

    #[test]
    fn alias_removal_preserves_the_other_alias_and_unrelated_provider_data() {
        let mut current = HashMap::from([
            ("One".into(), emote("id", "One")),
            ("Two".into(), emote("id", "Two")),
            ("Other".into(), emote("other", "Other")),
        ]);
        assert!(apply_delta(
            &mut current,
            &EmoteDelta::Remove {
                id: "id".into(),
                code: Some("One".into())
            }
        ));
        assert!(!current.contains_key("One"));
        assert!(current.contains_key("Two"));
        assert!(current.contains_key("Other"));
    }

    #[test]
    fn authoritative_reconnect_snapshot_observes_an_offline_removal() {
        let now = Instant::now();
        let mut state = state();
        let mut current = HashMap::new();
        state.begin_epoch(1);
        state.observe(
            &mut current,
            1,
            EmoteDelta::Upsert {
                emote: emote("id", "Added"),
                old_code: None,
                preserve_overlay: false,
            },
            now,
        );
        state.begin_epoch(2);
        let ticket = state.ticket(2).unwrap();
        assert!(
            state
                .replace_snapshot(&mut current, ticket, Vec::new(), true, now)
                .changed
        );
        assert!(current.is_empty());
        assert!(state.journal.is_empty());
    }

    #[test]
    fn cached_snapshot_eventually_observes_an_offline_readd() {
        let now = Instant::now();
        let old = emote("id", "Back");
        let mut state = state();
        let mut current = HashMap::from([("Back".into(), old.clone())]);
        state.begin_epoch(1);
        state.observe(
            &mut current,
            1,
            EmoteDelta::Remove {
                id: "id".into(),
                code: None,
            },
            now,
        );
        state.begin_epoch(2);
        let mut ticket = state.ticket(2).unwrap();
        ticket.started_at = now + CACHED_SNAPSHOT_WINDOW;
        assert!(
            state
                .replace_snapshot(&mut current, ticket, vec![old], false, ticket.started_at)
                .changed
        );
        assert!(current.contains_key("Back"));
        assert!(state.journal.is_empty());
    }

    #[test]
    fn a_duplicate_received_after_snapshot_start_still_has_a_new_revision() {
        let now = Instant::now();
        let mut state = state();
        let mut current = HashMap::new();
        state.begin_epoch(1);
        let removal = EmoteDelta::Remove {
            id: "id".into(),
            code: None,
        };
        state.observe(&mut current, 1, removal.clone(), now);
        let ticket = state.ticket(1).unwrap();
        state.observe(&mut current, 1, removal, now + Duration::from_secs(1));
        state.replace_snapshot(&mut current, ticket, vec![emote("id", "Gone")], true, now);
        assert!(current.is_empty());
        assert_eq!(state.journal.len(), 1);
        assert!(state.journal[0].revision > ticket.revision);
    }

    #[test]
    fn a_snapshot_started_before_journal_overflow_cannot_claim_recovery() {
        let now = Instant::now();
        let mut state = state();
        let mut current = HashMap::new();
        state.begin_epoch(1);
        let ticket = state.ticket(1).unwrap();
        for i in 0..=MAX_JOURNAL {
            state.observe(
                &mut current,
                1,
                EmoteDelta::Remove {
                    id: i.to_string(),
                    code: None,
                },
                now,
            );
        }
        let result =
            state.replace_snapshot(&mut current, ticket, vec![emote("0", "Gone")], true, now);
        assert!(result.needs_snapshot);
        assert!(!result.changed);
        assert!(current.is_empty());
        assert!(state.overflowed_at.is_some());
    }

    #[test]
    fn stale_socket_epochs_and_a_full_journal_cannot_mutate_current_definitions() {
        let now = Instant::now();
        let mut state = state();
        let mut current = HashMap::new();
        state.begin_epoch(2);
        assert_eq!(
            state.observe(
                &mut current,
                1,
                EmoteDelta::Upsert {
                    emote: emote("id", "Old"),
                    old_code: None,
                    preserve_overlay: false,
                },
                now
            ),
            ApplyResult::default()
        );
        for i in 0..MAX_JOURNAL {
            state.observe(
                &mut current,
                2,
                EmoteDelta::Remove {
                    id: i.to_string(),
                    code: None,
                },
                now,
            );
        }
        let result = state.observe(
            &mut current,
            2,
            EmoteDelta::Upsert {
                emote: emote("new", "New"),
                old_code: None,
                preserve_overlay: false,
            },
            now,
        );
        assert!(result.needs_snapshot);
        assert!(!result.changed);
        assert!(current.is_empty());
        assert_eq!(state.journal.len(), MAX_JOURNAL);
        let ticket = state.ticket(2).unwrap();
        assert!(
            state
                .replace_snapshot(&mut current, ticket, vec![emote("new", "New")], true, now)
                .changed
        );
        assert!(state.journal.is_empty());
        assert!(state.overflowed_at.is_none());
    }
}

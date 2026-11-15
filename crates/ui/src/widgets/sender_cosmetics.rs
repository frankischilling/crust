//! Current Twitch sender appearances shared by chat, mentions, replies, and whispers.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crust_core::model::{Sender, SenderNamePaint};
use egui::{Color32, Context, Id};

const MAX_IDENTITIES: usize = 2048;
const MAX_ID_BYTES: usize = 128;
const MAX_LOGIN_BYTES: usize = 64;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Appearance {
    pub color: Option<Color32>,
    pub paint: Option<Arc<SenderNamePaint>>,
}

struct Entry {
    user_id: String,
    login: Option<String>,
    appearance: Option<Appearance>,
    authoritative: bool,
    older: Option<usize>,
    newer: Option<usize>,
}

#[derive(Default)]
struct Cache {
    identities: HashMap<String, usize>,
    aliases: HashMap<String, usize>,
    entries: Vec<Entry>,
    oldest: Option<usize>,
    newest: Option<usize>,
}

impl Cache {
    fn unlink(&mut self, index: usize) {
        let entry = &self.entries[index];
        let (older, newer) = (entry.older, entry.newer);
        if let Some(older) = older {
            self.entries[older].newer = newer;
        } else {
            self.oldest = newer;
        }
        if let Some(newer) = newer {
            self.entries[newer].older = older;
        } else {
            self.newest = older;
        }
        self.entries[index].older = None;
        self.entries[index].newer = None;
    }

    fn make_newest(&mut self, index: usize) {
        self.entries[index].older = self.newest;
        self.entries[index].newer = None;
        if let Some(newest) = self.newest {
            self.entries[newest].newer = Some(index);
        } else {
            self.oldest = Some(index);
        }
        self.newest = Some(index);
    }

    fn touch(&mut self, index: usize) {
        if self.newest != Some(index) {
            self.unlink(index);
            self.make_newest(index);
        }
    }

    fn identity(&mut self, user_id: &str) -> Option<usize> {
        if !valid_id(user_id) {
            return None;
        }
        if let Some(&index) = self.identities.get(user_id) {
            self.touch(index);
            return Some(index);
        }

        let entry = Entry {
            user_id: user_id.to_owned(),
            login: None,
            appearance: None,
            authoritative: false,
            older: None,
            newer: None,
        };
        let index = if self.entries.len() < MAX_IDENTITIES {
            let index = self.entries.len();
            self.entries.push(entry);
            index
        } else {
            let index = self.oldest.expect("a full cache has an oldest entry");
            self.unlink(index);
            let removed = &self.entries[index];
            self.identities.remove(&removed.user_id);
            if let Some(login) = &removed.login {
                if self.aliases.get(login) == Some(&index) {
                    self.aliases.remove(login);
                }
            }
            self.entries[index] = entry;
            index
        };
        self.identities.insert(user_id.to_owned(), index);
        self.make_newest(index);
        Some(index)
    }

    fn bind_alias(&mut self, index: usize, login: &str, current_identity: bool) {
        let Some(login) = normalize_login(login) else {
            return;
        };
        if self.entries[index].login.as_deref() == Some(login.as_ref()) {
            return;
        }
        // Retained messages may contain an old login. They can seed a missing
        // identity, but cannot undo a rename or claim another account's alias.
        if !current_identity
            && (self.entries[index].login.is_some()
                || self
                    .aliases
                    .get(login.as_ref())
                    .is_some_and(|owner| *owner != index))
        {
            return;
        }
        if let Some(previous) = self.entries[index].login.take() {
            if self.aliases.get(&previous) == Some(&index) {
                self.aliases.remove(&previous);
            }
        }
        if let Some(previous_owner) = self.aliases.insert(login.to_string(), index) {
            if previous_owner != index {
                self.entries[previous_owner].login = None;
            }
        }
        self.entries[index].login = Some(login.into_owned());
    }

    fn set_appearance(
        &mut self,
        index: usize,
        color: Option<&str>,
        paint: Option<&SenderNamePaint>,
    ) {
        let entry = &mut self.entries[index];
        let previous = entry.appearance.as_ref();
        let paint = if previous.is_some_and(|appearance| appearance.paint.as_deref() == paint) {
            previous.and_then(|appearance| appearance.paint.clone())
        } else {
            paint.map(|paint| Arc::new(paint.clone()))
        };
        entry.appearance = Some(Appearance {
            color: color.and_then(crate::theme::parse_hex_rgb_opaque),
            paint,
        });
    }

    fn remember_sender(&mut self, sender: &Sender) {
        let Some(index) = self.identity(&sender.user_id.0) else {
            return;
        };
        self.bind_alias(index, &sender.login, false);
        if !self.entries[index].authoritative {
            self.set_appearance(index, sender.color.as_deref(), sender.name_paint.as_ref());
        }
    }

    fn remember_identity(&mut self, user_id: &str, login: &str) {
        if let Some(index) = self.identity(user_id) {
            self.bind_alias(index, login, true);
        }
    }

    fn update(&mut self, user_id: &str, color: Option<&str>, paint: Option<&SenderNamePaint>) {
        if let Some(index) = self.identity(user_id) {
            self.set_appearance(index, color, paint);
            self.entries[index].authoritative = true;
        }
    }

    fn get(&mut self, user_id: Option<&str>, login: &str) -> Option<Appearance> {
        let index = if let Some(user_id) = user_id.filter(|id| !id.is_empty()) {
            // A supplied ID must never resolve to a different login owner.
            *self.identities.get(user_id)?
        } else {
            *self.aliases.get(normalize_login(login)?.as_ref())?
        };
        self.touch(index);
        self.entries[index].appearance.clone()
    }
}

fn valid_id(user_id: &str) -> bool {
    !user_id.is_empty()
        && user_id.len() <= MAX_ID_BYTES
        && user_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn normalize_login(login: &str) -> Option<Cow<'_, str>> {
    let login = login.trim();
    if login.is_empty()
        || login.len() > MAX_LOGIN_BYTES
        || !login
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }
    if login.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Some(Cow::Owned(login.to_ascii_lowercase()))
    } else {
        Some(Cow::Borrowed(login))
    }
}

fn cache_id() -> Id {
    Id::new("twitch_sender_cosmetics")
}

fn cache(ctx: &Context) -> Arc<Mutex<Cache>> {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<Arc<Mutex<Cache>>>(cache_id())
            .clone()
    })
}

/// Seed a Twitch sender snapshot without replacing a newer live appearance.
pub(crate) fn remember_sender(ctx: &Context, sender: &Sender) {
    cache(ctx).lock().unwrap().remember_sender(sender);
}

/// Bind a current Twitch ID/login, including when its live update arrived first.
pub(crate) fn remember_identity(ctx: &Context, user_id: &str, login: &str) {
    cache(ctx).lock().unwrap().remember_identity(user_id, login);
}

/// Replace the live appearance; a missing paint explicitly clears an older paint.
pub(crate) fn update(
    ctx: &Context,
    user_id: &str,
    color: Option<&str>,
    paint: Option<&SenderNamePaint>,
) {
    cache(ctx).lock().unwrap().update(user_id, color, paint);
}

/// Look up a Twitch appearance. A present result with no paint is authoritative.
pub(crate) fn get(ctx: &Context, user_id: Option<&str>, login: &str) -> Option<Appearance> {
    let cache = ctx.data(|data| data.get_temp::<Arc<Mutex<Cache>>>(cache_id()))?;
    let result = cache.lock().unwrap().get(user_id, login);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sender(user_id: &str, login: &str, color: &str) -> Sender {
        Sender {
            user_id: crust_core::UserId(user_id.to_owned()),
            login: login.to_owned(),
            display_name: login.to_owned(),
            original_display_name: None,
            color: Some(color.to_owned()),
            name_paint: Some(SenderNamePaint {
                function: "linear-gradient".to_owned(),
                stops: vec![crust_core::model::SenderNamePaintStop {
                    at: 0.0,
                    color: color.to_owned(),
                }],
                ..Default::default()
            }),
            badges: Vec::new(),
        }
    }

    #[test]
    fn live_updates_before_identity_survive_snapshot_registration_and_removal() {
        let ctx = Context::default();
        let snapshot = sender("42", "Painter", "#112233");
        let current = sender("42", "Painter", "#AABBCC");
        update(&ctx, "42", Some("#AABBCC"), current.name_paint.as_ref());
        assert!(get(&ctx, None, "painter").is_none());
        let before = get(&ctx, Some("42"), "").unwrap();
        remember_identity(&ctx, "42", "Painter");
        remember_sender(&ctx, &snapshot);
        assert_eq!(get(&ctx, None, " PAINTER ").unwrap(), before);

        update(&ctx, "42", None, None);
        remember_sender(&ctx, &snapshot);
        assert_eq!(
            get(&ctx, Some("42"), "Painter"),
            Some(Appearance::default())
        );
        assert_eq!(get(&ctx, None, "painter"), Some(Appearance::default()));

        update(&ctx, "42", Some("#AABBCC"), current.name_paint.as_ref());
        assert_eq!(get(&ctx, None, "painter"), Some(before));
    }

    #[test]
    fn identity_registration_does_not_invent_a_paint_removal() {
        let ctx = Context::default();
        remember_identity(&ctx, "42", "viewer");
        assert!(get(&ctx, Some("42"), "viewer").is_none());
        assert!(get(&ctx, None, "viewer").is_none());
        let snapshot = sender("42", "viewer", "#112233");
        remember_sender(&ctx, &snapshot);
        assert!(get(&ctx, None, "viewer").unwrap().paint.is_some());
    }

    #[test]
    fn aliases_follow_current_identities_and_old_rows_cannot_reclaim_them() {
        let mut cache = Cache::default();
        let first = sender("1", "former_name", "#112233");
        cache.remember_sender(&first);
        cache.remember_identity("1", "new_name");
        cache.remember_sender(&first);
        assert!(cache.get(None, "former_name").is_none());
        let first_appearance = cache.get(Some("1"), "").unwrap();
        assert_eq!(cache.get(None, "NEW_NAME"), Some(first_appearance.clone()));

        let second = sender("2", "new_name", "#445566");
        cache.remember_identity("2", "new_name");
        cache.remember_sender(&second);
        cache.remember_sender(&sender("1", "new_name", "#112233"));
        assert_eq!(
            cache.get(None, "new_name").unwrap().color,
            Some(Color32::from_rgb(0x44, 0x55, 0x66))
        );
        assert_eq!(cache.get(Some("1"), "new_name"), Some(first_appearance));
        assert!(cache.get(Some("unknown"), "new_name").is_none());
        assert_eq!(cache.get(Some(""), "new_name"), cache.get(None, "new_name"));
        assert!(cache.get(None, "former_name").is_none());
    }

    #[test]
    fn repeated_reads_and_identical_snapshots_share_the_paint_allocation() {
        let mut cache = Cache::default();
        let snapshot = sender("42", "viewer", "#112233");
        cache.remember_sender(&snapshot);
        let first = cache.get(None, "viewer").unwrap().paint.unwrap();
        for _ in 0..100 {
            cache.remember_sender(&snapshot);
            let next = cache.get(Some("42"), "viewer").unwrap().paint.unwrap();
            assert!(Arc::ptr_eq(&first, &next));
        }
        cache.update("42", Some("#112233"), snapshot.name_paint.as_ref());
        assert!(Arc::ptr_eq(
            &first,
            &cache.get(None, "viewer").unwrap().paint.unwrap()
        ));
        cache.update("42", None, None);
        assert!(cache.get(None, "viewer").unwrap().paint.is_none());
        assert_eq!(first.stops[0].color, "#112233");
    }

    #[test]
    fn identities_aliases_and_recency_metadata_stay_bounded_under_churn() {
        let mut cache = Cache::default();
        for user in 0..MAX_IDENTITIES {
            cache.remember_identity(&user.to_string(), &format!("viewer_{user}"));
            cache.update(&user.to_string(), None, None);
        }
        cache.get(Some("0"), "");
        cache.remember_identity("replacement", "new_viewer");
        assert!(
            cache.identities.contains_key("0"),
            "a recent hit must survive eviction"
        );
        assert!(!cache.identities.contains_key("1"));
        assert!(!cache.aliases.contains_key("viewer_1"));
        for user in MAX_IDENTITIES..MAX_IDENTITIES * 4 {
            cache.remember_identity(&user.to_string(), &format!("viewer_{user}"));
            cache.get(None, &format!("viewer_{user}"));
        }
        assert_eq!(cache.entries.len(), MAX_IDENTITIES);
        assert_eq!(cache.identities.len(), MAX_IDENTITIES);
        assert_eq!(cache.aliases.len(), MAX_IDENTITIES);

        let mut visited = std::collections::HashSet::new();
        let (mut next, mut previous) = (cache.oldest, None);
        while let Some(index) = next {
            assert!(
                visited.insert(index),
                "recency links must not contain cycles"
            );
            let entry = &cache.entries[index];
            assert_eq!(entry.older, previous);
            assert_eq!(cache.identities.get(&entry.user_id), Some(&index));
            assert_eq!(
                cache.aliases.get(entry.login.as_ref().unwrap()),
                Some(&index)
            );
            previous = Some(index);
            next = entry.newer;
        }
        assert_eq!(visited.len(), MAX_IDENTITIES);
        assert_eq!(previous, cache.newest);

        for alias in 0..MAX_IDENTITIES * 2 {
            cache.remember_identity("renamed", &format!("alias_{alias}"));
        }
        assert_eq!(cache.aliases.len(), MAX_IDENTITIES);
        assert!(!cache.aliases.contains_key("alias_0"));
        assert!(cache
            .aliases
            .contains_key(&format!("alias_{}", MAX_IDENTITIES * 2 - 1)));
    }

    #[test]
    fn invalid_and_oversized_identity_fields_cannot_grow_the_cache() {
        let mut cache = Cache::default();
        for id in [
            "",
            "irc:someone",
            "with space",
            &"a".repeat(MAX_ID_BYTES + 1),
        ] {
            cache.update(id, None, None);
        }
        assert!(cache.entries.is_empty());
        for login in ["", "with space", "🙂", &"a".repeat(MAX_LOGIN_BYTES + 1)] {
            cache.remember_identity("42", login);
        }
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.aliases.is_empty());
    }
}

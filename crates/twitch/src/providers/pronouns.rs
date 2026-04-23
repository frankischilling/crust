//! alejo.io pronouns provider (HTTP GET + in-memory TTL cache).
//!
//! Chatterino parity: the public API exposes two endpoints:
//! - `GET /v1/pronouns` → map of `pronoun_id` → `{subject, object, singular}`.
//! - `GET /v1/users/:login` → `{pronoun_id: "..."}` or 404 when unset.
//!
//! We hit the map once lazily, then per-user lookup on demand.  Both positive
//! hits and 404 "unspecified" misses are cached to avoid repeat fetches.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use tracing::{debug, info, warn};

const API_PRONOUNS: &str = "https://api.pronouns.alejo.io/v1/pronouns";
const API_USERS: &str = "https://api.pronouns.alejo.io/v1/users";
/// Cache TTL - pronoun choices change rarely, so a generous window keeps
/// repeat usercard opens instant without blocking on the network.
const CACHE_TTL: Duration = Duration::from_secs(60 * 60 * 6);
/// Request timeout for a single HTTP call (per chatterino's network defaults).
const REQUEST_TIMEOUT: Duration = Duration::from_millis(4_500);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Cached {
    /// Resolved pronoun representation (e.g. `"he/him"`).
    Present(String),
    /// User has no pronouns set (alejo returned 404).
    Unspecified,
}

#[derive(Clone)]
struct CacheEntry {
    at: Instant,
    value: Cached,
}

/// Shared pronouns lookup handle.  Cheap to clone (`Arc` inside).
#[derive(Clone, Default)]
pub struct PronounsProvider {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    /// `pronoun_id` → display string, fetched from `/v1/pronouns`.
    catalog: RwLock<HashMap<String, String>>,
    /// Per-user lookup cache.
    users: RwLock<HashMap<String, CacheEntry>>,
    /// Whether the catalog fetch has succeeded at least once.
    catalog_loaded: RwLock<bool>,
}

impl PronounsProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fetch pronouns for `username`.  Returns `Some("he/him")` on hit,
    /// `None` for unspecified / network failure.
    ///
    /// Cache semantics:
    /// - Hit within TTL: returns cached result without network.
    /// - 404 is cached as `Unspecified` (same TTL).
    /// - Transient network errors are not cached - the next call will retry.
    pub async fn fetch_user(&self, username: &str) -> Option<String> {
        let key = username.trim().to_ascii_lowercase();
        if key.is_empty() {
            return None;
        }

        // Cache lookup
        {
            let cache = self.inner.users.read().await;
            if let Some(entry) = cache.get(&key) {
                if entry.at.elapsed() < CACHE_TTL {
                    return match &entry.value {
                        Cached::Present(s) => {
                            debug!("alejo cache hit: {key} -> {s}");
                            Some(s.clone())
                        }
                        Cached::Unspecified => {
                            debug!("alejo cache hit: {key} -> (unspecified)");
                            None
                        }
                    };
                }
            }
        }

        // Ensure catalog is loaded
        if !*self.inner.catalog_loaded.read().await {
            self.load_catalog().await;
        }

        let client = match build_client() {
            Ok(c) => c,
            Err(e) => {
                warn!("alejo: reqwest client build failed: {e}");
                return None;
            }
        };
        let url = format!("{API_USERS}/{key}");
        debug!("alejo GET {url}");
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!("alejo request failed for {key}: {e}");
                return None;
            }
        };

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            info!("alejo: no pronouns set for {key}");
            self.store(&key, Cached::Unspecified).await;
            return None;
        }
        if !resp.status().is_success() {
            warn!("alejo returned HTTP {} for {key}", resp.status());
            return None;
        }
        let json: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                warn!("alejo JSON parse failed for {key}: {e}");
                return None;
            }
        };
        let pronoun_id = json
            .get("pronoun_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        if pronoun_id.is_empty() {
            info!("alejo: empty pronoun_id for {key}");
            self.store(&key, Cached::Unspecified).await;
            return None;
        }
        let catalog = self.inner.catalog.read().await;
        let display = catalog.get(&pronoun_id).cloned();
        drop(catalog);

        match display {
            Some(s) => {
                info!("alejo: {key} -> {s} (id={pronoun_id})");
                self.store(&key, Cached::Present(s.clone())).await;
                Some(s)
            }
            None => {
                warn!("alejo: unknown pronoun_id {pronoun_id} for {key} (catalog stale?)");
                // Unknown id - don't cache so a later catalog refresh can resolve it.
                None
            }
        }
    }

    async fn store(&self, key: &str, value: Cached) {
        let mut cache = self.inner.users.write().await;
        cache.insert(
            key.to_owned(),
            CacheEntry {
                at: Instant::now(),
                value,
            },
        );
    }

    /// Populate the `pronoun_id` → display-string catalog from alejo's `/v1/pronouns`.
    /// Safe to call multiple times; later successful calls refresh the map.
    pub async fn load_catalog(&self) {
        debug!("alejo: loading catalog from {API_PRONOUNS}");
        let client = match build_client() {
            Ok(c) => c,
            Err(e) => {
                warn!("alejo catalog: client build failed: {e}");
                return;
            }
        };
        let resp = match client.get(API_PRONOUNS).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!("alejo catalog request failed: {e}");
                return;
            }
        };
        if !resp.status().is_success() {
            warn!("alejo catalog HTTP {}", resp.status());
            return;
        }
        let root: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                warn!("alejo catalog JSON parse failed: {e}");
                return;
            }
        };
        let obj = match root.as_object() {
            Some(o) => o,
            None => {
                warn!("alejo catalog: root not an object");
                return;
            }
        };

        let mut new_catalog = HashMap::with_capacity(obj.len());
        for (id, raw) in obj.iter() {
            let subject = raw.get("subject").and_then(|v| v.as_str()).unwrap_or("");
            let object = raw.get("object").and_then(|v| v.as_str()).unwrap_or("");
            let singular = raw
                .get("singular")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if subject.is_empty() || object.is_empty() {
                continue;
            }
            let display = if singular {
                subject.to_owned()
            } else {
                format!("{subject}/{object}")
            };
            new_catalog.insert(id.clone(), display);
        }

        if new_catalog.is_empty() {
            warn!("alejo catalog came back empty");
            return;
        }
        let count = new_catalog.len();
        {
            let mut catalog = self.inner.catalog.write().await;
            *catalog = new_catalog;
        }
        *self.inner.catalog_loaded.write().await = true;
        info!("alejo catalog loaded: {count} pronoun entries");
    }

    /// Test helper: is `username` present in the in-memory cache?
    #[cfg(test)]
    pub async fn is_cached(&self, username: &str) -> bool {
        let cache = self.inner.users.read().await;
        cache.contains_key(&username.to_ascii_lowercase())
    }

    /// Test helper: inject a catalog entry without hitting the network.
    #[cfg(test)]
    pub async fn seed_catalog(&self, entries: &[(&str, &str)]) {
        let mut catalog = self.inner.catalog.write().await;
        for (id, display) in entries {
            catalog.insert((*id).to_owned(), (*display).to_owned());
        }
        *self.inner.catalog_loaded.write().await = true;
    }

    /// Test helper: inject a resolved pronoun into the per-user cache.
    #[cfg(test)]
    pub async fn seed_user(&self, username: &str, display: &str) {
        self.store(
            &username.to_ascii_lowercase(),
            Cached::Present(display.to_owned()),
        )
        .await;
    }
}

fn build_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_username_returns_none() {
        let p = PronounsProvider::new();
        assert!(p.fetch_user("").await.is_none());
        assert!(p.fetch_user("   ").await.is_none());
    }

    #[tokio::test]
    async fn seeded_cache_returns_without_network() {
        let p = PronounsProvider::new();
        p.seed_catalog(&[("hh", "he/him")]).await;
        p.seed_user("Alice", "he/him").await;
        let got = p.fetch_user("alice").await;
        assert_eq!(got.as_deref(), Some("he/him"));
        assert!(p.is_cached("alice").await);
    }

    #[tokio::test]
    async fn cache_is_case_insensitive() {
        let p = PronounsProvider::new();
        p.seed_user("Bob", "they/them").await;
        let got = p.fetch_user("BOB").await;
        assert_eq!(got.as_deref(), Some("they/them"));
    }
}

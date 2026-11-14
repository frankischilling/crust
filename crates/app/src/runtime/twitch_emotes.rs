use std::collections::HashSet;
use std::future::Future;
use std::time::{Duration, Instant};

use crust_emotes::EmoteInfo;
use serde::Deserialize;
use tokio::task::JoinHandle;

const USER_EMOTES_URL: &str = "https://api.twitch.tv/helix/chat/emotes/user";
const EMOTES_SCOPE: &str = "user:read:emotes";
const PAGE_TIMEOUT: Duration = Duration::from_secs(10);
const LOAD_TIMEOUT: Duration = Duration::from_secs(45);
const REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);
const RETRY_INTERVAL: Duration = Duration::from_secs(30);
const MAX_PAGES: usize = 100;
const MAX_EMOTES: usize = 20_000;
const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug)]
enum LoadError {
    Transport,
    Http(u16),
    InvalidData,
    LimitExceeded,
    TimedOut,
    TaskFailed,
}

#[derive(Deserialize)]
struct EmotePage {
    data: Vec<UserEmote>,
    template: String,
    pagination: Pagination,
}

#[derive(Deserialize)]
struct Pagination {
    cursor: Option<String>,
}

#[derive(Deserialize)]
struct UserEmote {
    id: String,
    name: String,
    format: Vec<String>,
    scale: Vec<String>,
    theme_mode: Vec<String>,
}

impl UserEmote {
    fn into_info(self, template: &str) -> Result<EmoteInfo, LoadError> {
        if self.id.is_empty()
            || self.name.is_empty()
            || self.name.chars().any(char::is_whitespace)
            || ["{{id}}", "{{format}}", "{{theme_mode}}", "{{scale}}"]
                .iter()
                .any(|placeholder| !template.contains(*placeholder))
        {
            return Err(LoadError::InvalidData);
        }
        let format = ["animated", "static"]
            .into_iter()
            .find(|format| self.format.iter().any(|value| value.as_str() == *format))
            .ok_or(LoadError::InvalidData)?;
        let theme = ["dark", "light"]
            .into_iter()
            .find(|theme| self.theme_mode.iter().any(|value| value.as_str() == *theme))
            .ok_or(LoadError::InvalidData)?;
        let available = |scale: &str| self.scale.iter().any(|value| value == scale);
        let small = ["1.0", "2.0", "3.0"]
            .into_iter()
            .find(|scale| available(scale))
            .ok_or(LoadError::InvalidData)?;
        let url = |scale: &str| -> Result<String, LoadError> {
            let rendered = template
                .replace("{{id}}", &self.id)
                .replace("{{format}}", format)
                .replace("{{theme_mode}}", theme)
                .replace("{{scale}}", scale);
            let parsed = reqwest::Url::parse(&rendered).map_err(|_| LoadError::InvalidData)?;
            if parsed.scheme() != "https"
                || parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || rendered.contains("{{")
                || rendered.contains("}}")
            {
                return Err(LoadError::InvalidData);
            }
            Ok(rendered)
        };
        let url_1x = url(small)?;
        let url_2x = available("2.0").then(|| url("2.0")).transpose()?;
        let url_4x = available("3.0").then(|| url("3.0")).transpose()?;
        Ok(EmoteInfo {
            id: self.id,
            code: self.name,
            provider: "twitch".into(),
            url_1x,
            url_2x,
            url_4x,
        })
    }
}

async fn fetch_inventory_at(
    client: &reqwest::Client,
    url: &str,
    user_id: &str,
    client_id: &str,
    token: &str,
) -> Result<Vec<EmoteInfo>, LoadError> {
    let mut emotes = Vec::new();
    let mut emote_ids = HashSet::new();
    let mut cursors = HashSet::new();
    let mut after: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut request = client
            .get(url)
            .timeout(PAGE_TIMEOUT)
            .bearer_auth(token.strip_prefix("oauth:").unwrap_or(token))
            .header("Client-Id", client_id)
            .query(&[("user_id", user_id)]);
        if let Some(after) = &after {
            request = request.query(&[("after", after)]);
        }
        let mut response = request.send().await.map_err(|_| LoadError::Transport)?;
        if !response.status().is_success() {
            return Err(LoadError::Http(response.status().as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|bytes| bytes > MAX_PAGE_BYTES as u64)
        {
            return Err(LoadError::LimitExceeded);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| LoadError::Transport)? {
            if bytes.len().saturating_add(chunk.len()) > MAX_PAGE_BYTES {
                return Err(LoadError::LimitExceeded);
            }
            bytes.extend_from_slice(&chunk);
        }
        let page: EmotePage = serde_json::from_slice(&bytes).map_err(|_| LoadError::InvalidData)?;
        for emote in page.data {
            if emote_ids.insert(emote.id.clone()) {
                if emotes.len() >= MAX_EMOTES {
                    return Err(LoadError::LimitExceeded);
                }
                emotes.push(emote.into_info(&page.template)?);
            }
        }
        after = page.pagination.cursor.filter(|cursor| !cursor.is_empty());
        match &after {
            None => return Ok(emotes),
            Some(cursor) if cursor.len() > 4096 || !cursors.insert(cursor.clone()) => {
                return Err(LoadError::InvalidData);
            }
            Some(_) => {}
        }
    }
    Err(LoadError::LimitExceeded)
}

struct InventoryRequest {
    user_id: String,
    generation: u64,
    task: JoinHandle<Result<Vec<EmoteInfo>, LoadError>>,
}

/// Keep one private inventory request alive for the validated account.
#[derive(Default)]
pub(crate) struct TwitchEmoteInventory {
    owner: Option<String>,
    request: Option<InventoryRequest>,
    next_request: Option<Instant>,
}

impl TwitchEmoteInventory {
    /// Returns whether losing permission removed an existing inventory.
    pub(crate) fn sync(
        &mut self,
        user_id: Option<&str>,
        client_id: Option<&str>,
        token: &str,
        scopes: &[String],
        index: &crate::EmoteIndex,
    ) -> bool {
        let Some(user_id) = user_id.filter(|id| !id.is_empty()) else {
            self.clear();
            return false;
        };
        if !scopes.iter().any(|scope| scope == EMOTES_SCOPE) {
            self.clear();
            return index.write().unwrap().clear_twitch_inventory(user_id);
        }
        let Some(client_id) = client_id.filter(|id| !id.is_empty()) else {
            return false;
        };
        if token.trim().is_empty() {
            return false;
        }
        if self.owner.as_deref() == Some(user_id)
            && (self.request.is_some()
                || self.next_request.is_some_and(|time| Instant::now() < time))
        {
            return false;
        }
        let owned_user = user_id.to_owned();
        let client_id = client_id.to_owned();
        let token = token.to_owned();
        self.start_at(user_id, index, Instant::now(), async move {
            let client = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| LoadError::Transport)?;
            tokio::time::timeout(
                LOAD_TIMEOUT,
                fetch_inventory_at(&client, USER_EMOTES_URL, &owned_user, &client_id, &token),
            )
            .await
            .map_err(|_| LoadError::TimedOut)?
        });
        false
    }

    fn start_at<F>(
        &mut self,
        user_id: &str,
        index: &crate::EmoteIndex,
        now: Instant,
        load: F,
    ) -> bool
    where
        F: Future<Output = Result<Vec<EmoteInfo>, LoadError>> + Send + 'static,
    {
        if self.owner.as_deref() != Some(user_id) {
            self.clear();
            self.owner = Some(user_id.to_owned());
        }
        if self.request.is_some() || self.next_request.is_some_and(|time| now < time) {
            return false;
        }
        let Some(generation) = index.write().unwrap().begin_twitch_inventory(user_id) else {
            return false;
        };
        self.request = Some(InventoryRequest {
            user_id: user_id.to_owned(),
            generation,
            task: tokio::spawn(load),
        });
        true
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.request.is_some()
    }

    pub(crate) async fn complete_next(&mut self, index: &crate::EmoteIndex) -> bool {
        let Some(request) = self.request.as_mut() else {
            return false;
        };
        let result = (&mut request.task)
            .await
            .unwrap_or(Err(LoadError::TaskFailed));
        let request = self.request.take().unwrap();
        let emotes = match result {
            Ok(emotes) => {
                self.next_request = Some(Instant::now() + REFRESH_INTERVAL);
                Some(emotes)
            }
            Err(error) => {
                self.next_request = Some(Instant::now() + RETRY_INTERVAL);
                match error {
                    LoadError::Http(status) => {
                        tracing::warn!("Twitch emote inventory request failed (HTTP {status})")
                    }
                    error => tracing::warn!("Twitch emote inventory request failed: {error:?}"),
                }
                None
            }
        };
        index.write().unwrap().complete_twitch_inventory(
            &request.user_id,
            request.generation,
            emotes,
        )
    }

    pub(crate) fn clear(&mut self) {
        if let Some(request) = self.request.take() {
            request.task.abort();
        }
        self.owner = None;
        self.next_request = None;
    }
}

impl Drop for TwitchEmoteInventory {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, RwLock};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const TEMPLATE: &str =
        "https://static-cdn.jtvnw.net/emoticons/v2/{{id}}/{{format}}/{{theme_mode}}/{{scale}}";

    fn emote(id: &str, code: &str) -> serde_json::Value {
        serde_json::json!({"id": id, "name": code, "emote_type": "subscriptions",
            "emote_set_id": "set", "owner_id": "broadcaster",
            "format": ["static", "animated"], "scale": ["1.0", "2.0", "3.0"],
            "theme_mode": ["light", "dark"]})
    }

    fn page(data: Vec<serde_json::Value>, cursor: Option<&str>) -> String {
        serde_json::json!({"data": data, "template": TEMPLATE,
            "pagination": cursor.map(|cursor| serde_json::json!({"cursor": cursor})).unwrap_or(serde_json::json!({}))})
            .to_string()
    }

    fn info(id: &str, code: &str) -> EmoteInfo {
        serde_json::from_value::<UserEmote>(emote(id, code))
            .unwrap()
            .into_info(TEMPLATE)
            .unwrap()
    }

    #[test]
    fn image_urls_follow_advertised_formats_themes_and_scales() {
        let limited = serde_json::json!({"id": "static", "name": "StaticWave",
            "format": ["static"], "scale": ["2.0"], "theme_mode": ["light"]});
        let parsed: UserEmote = serde_json::from_value(limited.clone()).unwrap();
        let info = parsed.into_info(TEMPLATE).unwrap();
        assert_eq!(
            info.url_1x,
            "https://static-cdn.jtvnw.net/emoticons/v2/static/static/light/2.0"
        );
        assert!(info.url_4x.is_none());
        let parsed: UserEmote = serde_json::from_value(limited).unwrap();
        assert!(matches!(
            parsed.into_info("https://example.test/missing-template"),
            Err(LoadError::InvalidData)
        ));
    }

    async fn server(responses: Vec<(u16, String)>) -> (String, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/helix/chat/emotes/user",
            listener.local_addr().unwrap()
        );
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut socket, _) =
                    tokio::time::timeout(Duration::from_secs(3), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut buffer = [0; 1024];
                    let read =
                        tokio::time::timeout(Duration::from_secs(3), socket.read(&mut buffer))
                            .await
                            .unwrap()
                            .unwrap();
                    assert!(read > 0 && request.len() < 16_384);
                    request.extend_from_slice(&buffer[..read]);
                }
                requests.push(String::from_utf8(request).unwrap());
                socket.write_all(format!("HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            requests
        });
        (url, task)
    }

    #[tokio::test]
    async fn paginated_inventory_uses_validated_credentials_and_available_cdn_variants() {
        let (url, server) = server(vec![
            (200, page(vec![emote("first", "First")], Some("next /+"))),
            (
                200,
                page(
                    vec![emote("first", "First"), emote("second", "Second")],
                    None,
                ),
            ),
        ])
        .await;
        let inventory = fetch_inventory_at(
            &reqwest::Client::new(),
            &url,
            "42",
            "validated-client",
            "oauth:test-token",
        )
        .await
        .unwrap();
        assert_eq!(inventory.len(), 2);
        assert_eq!(
            inventory[0].url_1x,
            "https://static-cdn.jtvnw.net/emoticons/v2/first/animated/dark/1.0"
        );
        assert_eq!(
            inventory[0].url_4x.as_deref(),
            Some("https://static-cdn.jtvnw.net/emoticons/v2/first/animated/dark/3.0")
        );
        let requests = tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        for (page, request) in requests.iter().enumerate() {
            let lower = request.to_ascii_lowercase();
            assert!(lower.contains("authorization: bearer test-token\r\n"));
            assert!(lower.contains("client-id: validated-client\r\n"));
            let target = request.split_whitespace().nth(1).unwrap();
            let target = reqwest::Url::parse(&format!("http://test{target}")).unwrap();
            let params: std::collections::HashMap<_, _> =
                target.query_pairs().into_owned().collect();
            assert_eq!(params.get("user_id").map(String::as_str), Some("42"));
            assert_eq!(
                params.get("after").map(String::as_str),
                (page == 1).then_some("next /+")
            );
            assert!(!params.contains_key("broadcaster_id"));
        }
    }

    #[tokio::test]
    async fn failed_later_pages_and_repeated_cursors_never_return_partial_inventory() {
        let (url, task) = server(vec![
            (200, page(vec![emote("first", "First")], Some("next"))),
            (401, "{}".into()),
        ])
        .await;
        assert!(matches!(
            fetch_inventory_at(&reqwest::Client::new(), &url, "42", "client", "token").await,
            Err(LoadError::Http(401))
        ));
        task.await.unwrap();
        let (url, task) = server(vec![
            (200, page(Vec::new(), Some("cycle"))),
            (200, page(Vec::new(), Some("cycle"))),
        ])
        .await;
        assert!(matches!(
            fetch_inventory_at(&reqwest::Client::new(), &url, "42", "client", "token").await,
            Err(LoadError::InvalidData)
        ));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn valid_empty_inventory_is_distinct_from_a_malformed_response() {
        for (body, valid) in [(page(Vec::new(), None), true), ("{}".into(), false)] {
            let (url, task) = server(vec![(200, body)]).await;
            let result =
                fetch_inventory_at(&reqwest::Client::new(), &url, "42", "client", "token").await;
            if valid {
                assert!(result.unwrap().is_empty());
            } else {
                assert!(matches!(result, Err(LoadError::InvalidData)));
            }
            task.await.unwrap();
        }
    }

    fn index(owner: &str) -> crate::EmoteIndex {
        let mut registry = super::super::emote_registry::EmoteRegistry::default();
        registry.set_account(Some(owner));
        Arc::new(RwLock::new(registry))
    }

    async fn complete(requests: &mut TwitchEmoteInventory, index: &crate::EmoteIndex) -> bool {
        tokio::time::timeout(Duration::from_secs(3), requests.complete_next(index))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn failed_refresh_preserves_inventory_and_successful_empty_refresh_clears_it() {
        let index = index("alice");
        let mut requests = TwitchEmoteInventory::default();
        let now = Instant::now();
        assert!(requests.start_at("alice", &index, now, async {
            Ok(vec![info("old", "SubLove")])
        }));
        assert!(complete(&mut requests, &index).await);
        assert!(!requests.start_at("alice", &index, now, async { Ok(Vec::new()) }));
        let due = requests.next_request.unwrap();
        assert!(requests.start_at("alice", &index, due, async { Err(LoadError::Transport) }));
        assert!(!complete(&mut requests, &index).await);
        assert_eq!(
            index
                .read()
                .unwrap()
                .twitch_for_user("alice", "SubLove")
                .unwrap()
                .id,
            "old"
        );
        let due = requests.next_request.unwrap();
        assert!(requests.start_at("alice", &index, due, async { Ok(Vec::new()) }));
        assert!(complete(&mut requests, &index).await);
        assert!(index
            .read()
            .unwrap()
            .twitch_for_user("alice", "SubLove")
            .is_none());
    }

    #[tokio::test]
    async fn switching_accounts_cancels_old_work_and_stale_completions_cannot_publish() {
        let index = index("alice");
        let mut requests = TwitchEmoteInventory::default();
        let (mut completion, pending) = tokio::sync::oneshot::channel();
        assert!(requests.start_at("alice", &index, Instant::now(), async {
            pending.await.unwrap()
        }));
        let old_generation = requests.request.as_ref().unwrap().generation;
        index.write().unwrap().set_account(Some("bob"));
        assert!(requests.start_at("bob", &index, Instant::now(), async {
            Ok(vec![info("bob", "BobWave")])
        }));
        tokio::time::timeout(Duration::from_secs(3), completion.closed())
            .await
            .unwrap();
        assert!(complete(&mut requests, &index).await);
        assert!(!index.write().unwrap().complete_twitch_inventory(
            "alice",
            old_generation,
            Some(vec![info("alice", "AliceWave")])
        ));
        let catalog = index.read().unwrap().catalog();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].user_id.as_ref().unwrap().0, "bob");
        assert_eq!(catalog[0].code, "BobWave");

        // Even a completed job is rejected if the account changes before reduction.
        requests.clear();
        assert!(requests.start_at("bob", &index, Instant::now(), async {
            Ok(vec![info("stale", "Stale")])
        }));
        index.write().unwrap().set_account(Some("carol"));
        assert!(!complete(&mut requests, &index).await);
        assert!(index.read().unwrap().catalog().is_empty());
    }

    #[tokio::test]
    async fn missing_scope_does_not_request_inventory_or_remove_public_emotes() {
        let index = index("alice");
        {
            let mut registry = index.write().unwrap();
            registry.replace_global("twitch", vec![info("global", "Kappa")]);
            let generation = registry.begin_twitch_inventory("alice").unwrap();
            assert!(registry.complete_twitch_inventory(
                "alice",
                generation,
                Some(vec![info("private", "SubLove")])
            ));
        }
        let mut requests = TwitchEmoteInventory::default();
        assert!(requests.sync(Some("alice"), Some("client"), "token", &[], &index));
        assert!(!requests.has_pending());
        assert!(!requests.sync(Some("alice"), Some("client"), "token", &[], &index));
        let registry = index.read().unwrap();
        assert!(registry.twitch_for_user("alice", "SubLove").is_none());
        assert_eq!(
            registry.twitch_for_user("alice", "Kappa").unwrap().id,
            "global"
        );
    }
}

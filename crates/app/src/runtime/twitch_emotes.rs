use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;
use std::time::{Duration, Instant};

use crust_core::model::ChannelId;
use crust_emotes::EmoteInfo;
use serde::{de::DeserializeOwned, Deserialize};
use tokio::task::JoinHandle;

use super::emote_registry::TwitchFollowerTarget;

const USER_EMOTES_URL: &str = "https://api.twitch.tv/helix/chat/emotes/user";
const GLOBAL_EMOTES_URL: &str = "https://api.twitch.tv/helix/chat/emotes/global";
const EMOTES_SCOPE: &str = "user:read:emotes";
const PAGE_TIMEOUT: Duration = Duration::from_secs(10);
const LOAD_TIMEOUT: Duration = Duration::from_secs(45);
const REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);
const RETRY_INTERVAL: Duration = Duration::from_secs(30);
const MAX_PAGES: usize = 100;
const MAX_EMOTES: usize = 20_000;
const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_REQUESTS: usize = 4;
const MAX_FOLLOWER_EMOTES: usize = 256;

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
struct GlobalEmoteCatalog {
    data: Vec<UserEmote>,
    template: String,
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
    #[serde(default)]
    emote_type: String,
    #[serde(default)]
    owner_id: String,
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
            zero_width: false,
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
        let page = fetch_page_at(
            client,
            url,
            user_id,
            client_id,
            token,
            None,
            after.as_deref(),
        )
        .await?;
        for emote in page.data {
            if emote_ids.insert(emote.id.clone()) {
                if emote_ids.len() > MAX_EMOTES {
                    return Err(LoadError::LimitExceeded);
                }
                if emote.emote_type != "follower" {
                    emotes.push(emote.into_info(&page.template)?);
                }
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

async fn fetch_page_at(
    client: &reqwest::Client,
    url: &str,
    user_id: &str,
    client_id: &str,
    token: &str,
    broadcaster_id: Option<&str>,
    after: Option<&str>,
) -> Result<EmotePage, LoadError> {
    let mut request = client
        .get(url)
        .timeout(PAGE_TIMEOUT)
        .bearer_auth(token.trim().strip_prefix("oauth:").unwrap_or(token.trim()))
        .header("Client-Id", client_id)
        .query(&[("user_id", user_id)]);
    if let Some(broadcaster_id) = broadcaster_id {
        request = request.query(&[("broadcaster_id", broadcaster_id)]);
    }
    if let Some(after) = after {
        request = request.query(&[("after", after)]);
    }
    fetch_response(request).await
}

async fn fetch_response<T: DeserializeOwned>(
    request: reqwest::RequestBuilder,
) -> Result<T, LoadError> {
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
    serde_json::from_slice(&bytes).map_err(|_| LoadError::InvalidData)
}

async fn fetch_global_catalog_at(
    client: &reqwest::Client,
    url: &str,
    client_id: &str,
    token: &str,
) -> Result<Vec<EmoteInfo>, LoadError> {
    let catalog: GlobalEmoteCatalog = fetch_response(
        client
            .get(url)
            .timeout(PAGE_TIMEOUT)
            .bearer_auth(token.trim().strip_prefix("oauth:").unwrap_or(token.trim()))
            .header("Client-Id", client_id),
    )
    .await?;
    let mut emotes = Vec::new();
    let mut ids = HashSet::new();
    for emote in catalog.data {
        if ids.insert(emote.id.clone()) {
            if emotes.len() >= MAX_EMOTES {
                return Err(LoadError::LimitExceeded);
            }
            emotes.push(emote.into_info(&catalog.template)?);
        }
    }
    Ok(emotes)
}

async fn fetch_follower_inventory_at(
    client: &reqwest::Client,
    url: &str,
    user_id: &str,
    client_id: &str,
    token: &str,
    broadcaster_id: &str,
) -> Result<Vec<EmoteInfo>, LoadError> {
    // The service adds follower emotes to the first page. Further pages repeat
    // the account inventory, which has its own paginated request.
    let page = fetch_page_at(
        client,
        url,
        user_id,
        client_id,
        token,
        Some(broadcaster_id),
        None,
    )
    .await?;
    let mut emotes = Vec::new();
    let mut ids = HashSet::new();
    for emote in page.data {
        if emote.emote_type != "follower" || emote.owner_id != broadcaster_id {
            continue;
        }
        if ids.insert(emote.id.clone()) {
            if emotes.len() >= MAX_FOLLOWER_EMOTES {
                return Err(LoadError::LimitExceeded);
            }
            emotes.push(emote.into_info(&page.template)?);
        }
    }
    Ok(emotes)
}

struct InventoryRequest {
    user_id: String,
    generation: u64,
    task: JoinHandle<Result<Vec<EmoteInfo>, LoadError>>,
}

struct FollowerRequest {
    target: TwitchFollowerTarget,
    request: InventoryRequest,
}

enum Completion {
    Global(Result<Vec<EmoteInfo>, LoadError>),
    Account(Result<Vec<EmoteInfo>, LoadError>),
    Follower(ChannelId, Result<Vec<EmoteInfo>, LoadError>),
}

/// Global catalogs, account inventory, and channel entitlements share a request budget.
#[derive(Default)]
pub(crate) struct TwitchEmoteInventory {
    owner: Option<String>,
    client: Option<reqwest::Client>,
    global_request: Option<InventoryRequest>,
    next_global_request: Option<Instant>,
    request: Option<InventoryRequest>,
    next_request: Option<Instant>,
    follower_requests: HashMap<ChannelId, FollowerRequest>,
    follower_schedule: HashMap<ChannelId, (TwitchFollowerTarget, Instant)>,
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
        if self.owner.as_deref() != Some(user_id) {
            self.clear();
            self.owner = Some(user_id.to_owned());
        }
        let has_inventory_scope = scopes.iter().any(|scope| scope == EMOTES_SCOPE);
        let changed = if has_inventory_scope {
            false
        } else {
            self.clear_entitlements();
            index.write().unwrap().clear_twitch_inventory(user_id)
        };
        let Some(client_id) = client_id.filter(|id| !id.is_empty()) else {
            return changed;
        };
        if token.trim().is_empty() {
            return changed;
        }
        if self.client.is_none() {
            let Ok(client) = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()
            else {
                return changed;
            };
            self.client = Some(client);
        }
        let client = self.client.as_ref().unwrap();
        let now = Instant::now();
        if self.global_request.is_none()
            && self.active_requests() < MAX_REQUESTS
            && self.next_global_request.is_none_or(|due| now >= due)
        {
            let client = client.clone();
            let owned_client = client_id.to_owned();
            let owned_token = token.to_owned();
            self.start_global_at(user_id, index, now, async move {
                fetch_global_catalog_at(&client, GLOBAL_EMOTES_URL, &owned_client, &owned_token)
                    .await
            });
        }
        if !has_inventory_scope {
            return changed;
        }
        if self.request.is_none()
            && self.active_requests() < MAX_REQUESTS
            && self.next_request.is_none_or(|due| now >= due)
        {
            let client = self.client.as_ref().unwrap().clone();
            let owned_user = user_id.to_owned();
            let owned_client = client_id.to_owned();
            let owned_token = token.to_owned();
            self.start_at(user_id, index, now, async move {
                tokio::time::timeout(
                    LOAD_TIMEOUT,
                    fetch_inventory_at(
                        &client,
                        USER_EMOTES_URL,
                        &owned_user,
                        &owned_client,
                        &owned_token,
                    ),
                )
                .await
                .map_err(|_| LoadError::TimedOut)?
            });
        }
        let mut targets = index.read().unwrap().twitch_follower_targets();
        targets.sort_by(|left, right| left.channel.as_str().cmp(right.channel.as_str()));
        self.retain_followers(&targets);
        for target in targets {
            if self.active_requests() >= MAX_REQUESTS {
                break;
            }
            if !self.follower_due(&target, now) {
                continue;
            }
            let client = self.client.as_ref().unwrap().clone();
            let owned_user = user_id.to_owned();
            let owned_client = client_id.to_owned();
            let owned_token = token.to_owned();
            let room_id = target.room_id.clone();
            self.start_follower_at(user_id, target, index, now, async move {
                tokio::time::timeout(
                    LOAD_TIMEOUT,
                    fetch_follower_inventory_at(
                        &client,
                        USER_EMOTES_URL,
                        &owned_user,
                        &owned_client,
                        &owned_token,
                        &room_id,
                    ),
                )
                .await
                .map_err(|_| LoadError::TimedOut)?
            });
        }
        changed
    }

    fn active_requests(&self) -> usize {
        self.follower_requests.len()
            + usize::from(self.request.is_some())
            + usize::from(self.global_request.is_some())
    }

    fn start_global_at<F>(
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
        if self.global_request.is_some()
            || self.active_requests() >= MAX_REQUESTS
            || self.next_global_request.is_some_and(|time| now < time)
        {
            return false;
        }
        let Some(generation) = index.write().unwrap().begin_twitch_globals(user_id) else {
            return false;
        };
        self.global_request = Some(InventoryRequest {
            user_id: user_id.to_owned(),
            generation,
            task: tokio::spawn(load),
        });
        true
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
        if self.request.is_some()
            || self.active_requests() >= MAX_REQUESTS
            || self.next_request.is_some_and(|time| now < time)
        {
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

    fn retain_followers(&mut self, targets: &[TwitchFollowerTarget]) {
        self.follower_requests.retain(|_, request| {
            if targets.contains(&request.target) {
                true
            } else {
                request.request.task.abort();
                false
            }
        });
        self.follower_schedule
            .retain(|_, (target, _)| targets.contains(target));
    }

    fn start_follower_at<F>(
        &mut self,
        user_id: &str,
        target: TwitchFollowerTarget,
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
        if self.active_requests() >= MAX_REQUESTS || !self.follower_due(&target, now) {
            return false;
        }
        let Some(generation) = index
            .write()
            .unwrap()
            .begin_twitch_follower_inventory(user_id, &target)
        else {
            return false;
        };
        self.follower_requests.insert(
            target.channel.clone(),
            FollowerRequest {
                target,
                request: InventoryRequest {
                    user_id: user_id.to_owned(),
                    generation,
                    task: tokio::spawn(load),
                },
            },
        );
        true
    }

    fn follower_due(&self, target: &TwitchFollowerTarget, now: Instant) -> bool {
        !self.follower_requests.contains_key(&target.channel)
            && self
                .follower_schedule
                .get(&target.channel)
                .is_none_or(|(previous, due)| previous != target || now >= *due)
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.active_requests() != 0
    }

    pub(crate) async fn complete_next(&mut self, index: &crate::EmoteIndex) -> bool {
        let Some(completion) = futures_util::future::poll_fn(|cx| {
            if let Some(request) = self.global_request.as_mut() {
                if let Poll::Ready(result) = Pin::new(&mut request.task).poll(cx) {
                    return Poll::Ready(Some(Completion::Global(
                        result.unwrap_or(Err(LoadError::TaskFailed)),
                    )));
                }
            }
            if let Some(request) = self.request.as_mut() {
                if let Poll::Ready(result) = Pin::new(&mut request.task).poll(cx) {
                    return Poll::Ready(Some(Completion::Account(
                        result.unwrap_or(Err(LoadError::TaskFailed)),
                    )));
                }
            }
            for (channel, request) in &mut self.follower_requests {
                if let Poll::Ready(result) = Pin::new(&mut request.request.task).poll(cx) {
                    return Poll::Ready(Some(Completion::Follower(
                        channel.clone(),
                        result.unwrap_or(Err(LoadError::TaskFailed)),
                    )));
                }
            }
            if self.has_pending() {
                Poll::Pending
            } else {
                Poll::Ready(None)
            }
        })
        .await
        else {
            return false;
        };
        let (request, target, is_global, result) = match completion {
            Completion::Global(result) => (self.global_request.take().unwrap(), None, true, result),
            Completion::Account(result) => (self.request.take().unwrap(), None, false, result),
            Completion::Follower(channel, result) => {
                let request = self.follower_requests.remove(&channel).unwrap();
                (request.request, Some(request.target), false, result)
            }
        };
        let next_request = Instant::now()
            + if result.is_ok() {
                REFRESH_INTERVAL
            } else {
                RETRY_INTERVAL
            };
        if is_global {
            self.next_global_request = Some(next_request);
        } else if let Some(target) = &target {
            self.follower_schedule
                .insert(target.channel.clone(), (target.clone(), next_request));
        } else {
            self.next_request = Some(next_request);
        }
        let emotes = match result {
            Ok(emotes) => Some(emotes),
            Err(error) => {
                match error {
                    LoadError::Http(status) => {
                        tracing::warn!("Twitch emote inventory request failed (HTTP {status})")
                    }
                    error => tracing::warn!("Twitch emote inventory request failed: {error:?}"),
                }
                None
            }
        };
        let mut registry = index.write().unwrap();
        if is_global {
            let count = emotes.as_ref().map(Vec::len);
            let changed =
                registry.complete_twitch_globals(&request.user_id, request.generation, emotes);
            if let Some(count) = count.filter(|_| changed) {
                tracing::debug!(emotes = count, "Twitch global emote catalog updated");
            }
            changed
        } else if let Some(target) = target {
            registry.complete_twitch_follower_inventory(
                &request.user_id,
                &target,
                request.generation,
                emotes,
            )
        } else {
            registry.complete_twitch_inventory(&request.user_id, request.generation, emotes)
        }
    }

    pub(crate) fn clear(&mut self) {
        if let Some(request) = self.global_request.take() {
            request.task.abort();
        }
        self.next_global_request = None;
        self.clear_entitlements();
        self.owner = None;
    }

    fn clear_entitlements(&mut self) {
        if let Some(request) = self.request.take() {
            request.task.abort();
        }
        for (_, request) in self.follower_requests.drain() {
            request.request.task.abort();
        }
        self.follower_schedule.clear();
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

    fn follower_emote(id: &str, code: &str, owner: &str) -> serde_json::Value {
        let mut value = emote(id, code);
        value["emote_type"] = "follower".into();
        value["owner_id"] = owner.into();
        value
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
    async fn global_catalog_uses_credentials_without_user_queries_or_pagination() {
        let mut global = emote("unity", "TwitchUnity");
        global.as_object_mut().unwrap().remove("emote_type");
        global.as_object_mut().unwrap().remove("owner_id");
        let body = serde_json::json!({
            "data": [global.clone(), global],
            "template": TEMPLATE,
        })
        .to_string();
        let (url, server) = server(vec![(200, body)]).await;
        let url = url.replace("/user", "/global");
        let catalog = fetch_global_catalog_at(
            &reqwest::Client::new(),
            &url,
            "validated-client",
            " oauth:test-token ",
        )
        .await
        .unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].code, "TwitchUnity");
        assert!(catalog[0].url_1x.ends_with("/unity/animated/dark/1.0"));
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /helix/chat/emotes/global HTTP/1.1\r\n"));
        let headers = requests[0].to_ascii_lowercase();
        assert!(headers.contains("authorization: bearer test-token\r\n"));
        assert!(headers.contains("client-id: validated-client\r\n"));
    }

    #[tokio::test]
    async fn global_catalog_rejects_partial_or_failed_responses_and_accepts_empty_results() {
        let mut invalid = emote("invalid", "Invalid");
        invalid["format"] = serde_json::json!([]);
        for (status, body, valid) in [
            (
                200,
                serde_json::json!({"data": [], "template": TEMPLATE}).to_string(),
                true,
            ),
            (200, "{}".into(), false),
            (503, "{}".into(), false),
            (
                200,
                serde_json::json!({"data": [emote("valid", "Valid"), invalid], "template": TEMPLATE})
                    .to_string(),
                false,
            ),
        ] {
            let (url, task) = server(vec![(status, body)]).await;
            let result = fetch_global_catalog_at(&reqwest::Client::new(), &url, "client", "token")
                .await;
            if valid {
                assert!(result.unwrap().is_empty());
            } else {
                assert!(result.is_err());
            }
            task.await.unwrap();
        }
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
    async fn follower_query_uses_only_the_first_page_and_matching_channel_entitlements() {
        let (url, server) = server(vec![(
            200,
            page(
                vec![
                    emote("sub", "AccountSub"),
                    follower_emote("other", "OtherFollow", "999"),
                    follower_emote("follow", "ChannelFollow", "123"),
                    follower_emote("follow", "ChannelFollow", "123"),
                ],
                Some("account-next"),
            ),
        )])
        .await;
        let inventory = fetch_follower_inventory_at(
            &reqwest::Client::new(),
            &url,
            "42",
            "client",
            "oauth:token",
            "123",
        )
        .await
        .unwrap();
        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory[0].id, "follow");
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 1);
        let target = requests[0].split_whitespace().nth(1).unwrap();
        let target = reqwest::Url::parse(&format!("http://test{target}")).unwrap();
        let params: HashMap<_, _> = target.query_pairs().into_owned().collect();
        assert_eq!(
            params.get("broadcaster_id").map(String::as_str),
            Some("123")
        );
        assert_eq!(params.get("user_id").map(String::as_str), Some("42"));
        assert!(!params.contains_key("after"));
        assert!(requests[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer token\r\n"));
    }

    #[tokio::test]
    async fn follower_responses_never_enter_the_account_wide_inventory() {
        let (url, task) = server(vec![(
            200,
            page(
                vec![
                    follower_emote("follow", "ChannelOnly", "123"),
                    emote("sub", "AccountSub"),
                ],
                None,
            ),
        )])
        .await;
        let inventory = fetch_inventory_at(&reqwest::Client::new(), &url, "42", "client", "token")
            .await
            .unwrap();
        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory[0].id, "sub");
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

    fn follower_target(index: &crate::EmoteIndex, room: &str) -> TwitchFollowerTarget {
        let channel = ChannelId::new(room);
        let mut registry = index.write().unwrap();
        registry.open_channel(&channel);
        registry.begin_channel_load(&channel, room, false).unwrap();
        registry
            .twitch_follower_targets()
            .into_iter()
            .find(|target| target.channel == channel)
            .unwrap()
    }

    #[tokio::test]
    async fn global_catalogs_share_the_budget_with_account_and_follower_inventory() {
        let index = index("alice");
        let targets: Vec<_> = (1..=3)
            .map(|room| follower_target(&index, &room.to_string()))
            .collect();
        let mut requests = TwitchEmoteInventory::default();
        let now = Instant::now();
        assert!(requests.start_global_at("alice", &index, now, std::future::pending()));
        assert!(requests.start_at("alice", &index, now, std::future::pending()));
        for target in &targets[..2] {
            assert!(
                requests.start_follower_at("alice", target.clone(), &index, now, async {
                    Ok(Vec::new())
                })
            );
        }
        assert_eq!(requests.active_requests(), MAX_REQUESTS);
        assert!(
            !requests.start_follower_at("alice", targets[2].clone(), &index, now, async {
                Ok(Vec::new())
            })
        );
        complete(&mut requests, &index).await;
        assert!(
            requests.start_follower_at("alice", targets[2].clone(), &index, now, async {
                Ok(Vec::new())
            })
        );
        requests.clear();
        assert!(!requests.has_pending());
    }

    #[tokio::test]
    async fn global_refresh_preserves_failures_retries_and_accepts_authoritative_empty_results() {
        let index = index("alice");
        let mut requests = TwitchEmoteInventory::default();
        let now = Instant::now();
        assert!(requests.start_global_at("alice", &index, now, async {
            Ok(vec![info("global", "TwitchUnity")])
        }));
        assert!(complete(&mut requests, &index).await);
        let catalog = index.read().unwrap().catalog();
        assert_eq!(catalog.len(), 1);
        assert!(catalog[0].channel.is_none());
        assert!(catalog[0].user_id.is_none());
        assert_eq!(catalog[0].scope, "global");
        assert!(!requests.start_global_at("alice", &index, now, async { Ok(Vec::new()) }));
        let due = requests.next_global_request.unwrap();
        assert!(requests.start_global_at("alice", &index, due, async { Err(LoadError::Transport) }));
        assert!(!complete(&mut requests, &index).await);
        assert_eq!(index.read().unwrap().catalog()[0].code, "TwitchUnity");
        let retry_at = requests.next_global_request.unwrap();
        assert!(!requests.start_global_at(
            "alice",
            &index,
            retry_at - Duration::from_millis(1),
            async { Ok(Vec::new()) }
        ));
        assert!(requests.start_global_at("alice", &index, retry_at, async { Ok(Vec::new()) }));
        assert!(complete(&mut requests, &index).await);
        assert!(index.read().unwrap().catalog().is_empty());
    }

    #[tokio::test]
    async fn account_switch_cancels_global_work_and_rejects_late_completions() {
        let index = index("alice");
        let mut requests = TwitchEmoteInventory::default();
        let (mut completion, pending) = tokio::sync::oneshot::channel();
        assert!(
            requests.start_global_at("alice", &index, Instant::now(), async {
                pending.await.unwrap()
            })
        );
        let generation = requests.global_request.as_ref().unwrap().generation;
        index.write().unwrap().set_account(Some("bob"));
        assert!(
            requests.start_global_at("bob", &index, Instant::now(), async {
                Ok(vec![info("new", "NewGlobal")])
            })
        );
        tokio::time::timeout(Duration::from_secs(3), completion.closed())
            .await
            .unwrap();
        assert!(complete(&mut requests, &index).await);
        assert!(!index.write().unwrap().complete_twitch_globals(
            "alice",
            generation,
            Some(vec![info("stale", "StaleGlobal")])
        ));
        assert_eq!(index.read().unwrap().catalog()[0].code, "NewGlobal");

        requests.clear();
        assert!(
            requests.start_global_at("bob", &index, Instant::now(), async {
                Ok(vec![info("late", "LateGlobal")])
            })
        );
        index.write().unwrap().set_account(None);
        assert!(!complete(&mut requests, &index).await);
        assert_eq!(index.read().unwrap().catalog()[0].code, "NewGlobal");
    }

    #[tokio::test]
    async fn follower_requests_share_the_budget_and_finish_without_waiting_for_account_inventory() {
        let index = index("alice");
        let targets: Vec<_> = (1..=4)
            .map(|room| follower_target(&index, &room.to_string()))
            .collect();
        let mut requests = TwitchEmoteInventory::default();
        assert!(requests.start_at("alice", &index, Instant::now(), std::future::pending()));
        for target in &targets[..3] {
            assert!(requests.start_follower_at(
                "alice",
                target.clone(),
                &index,
                Instant::now(),
                async { Ok(vec![info("follow", "ChannelFollow")]) }
            ));
        }
        assert!(!requests.start_follower_at(
            "alice",
            targets[3].clone(),
            &index,
            Instant::now(),
            async { Ok(Vec::new()) }
        ));
        assert!(complete(&mut requests, &index).await);
        assert!(requests
            .request
            .as_ref()
            .is_some_and(|request| !request.task.is_finished()));
        assert!(requests.start_follower_at(
            "alice",
            targets[3].clone(),
            &index,
            Instant::now(),
            async { Ok(Vec::new()) }
        ));
        requests.clear();
        assert!(!requests.has_pending());
    }

    #[tokio::test]
    async fn follower_refresh_retries_failures_and_clears_successful_empty_results() {
        let index = index("alice");
        let target = follower_target(&index, "123");
        let mut requests = TwitchEmoteInventory::default();
        let now = Instant::now();
        assert!(
            requests.start_follower_at("alice", target.clone(), &index, now, async {
                Ok(vec![info("follow", "ChannelFollow")])
            })
        );
        assert!(complete(&mut requests, &index).await);
        assert!(
            !requests.start_follower_at("alice", target.clone(), &index, now, async {
                Ok(Vec::new())
            })
        );
        let due = requests.follower_schedule[&target.channel].1;
        assert!(
            requests.start_follower_at("alice", target.clone(), &index, due, async {
                Err(LoadError::Transport)
            })
        );
        assert!(!complete(&mut requests, &index).await);
        assert!(index
            .read()
            .unwrap()
            .twitch_for_channel_user(&target.channel, "alice", "ChannelFollow")
            .is_some());
        let due = requests.follower_schedule[&target.channel].1;
        assert!(
            requests.start_follower_at("alice", target.clone(), &index, due, async {
                Ok(Vec::new())
            })
        );
        assert!(complete(&mut requests, &index).await);
        assert!(index
            .read()
            .unwrap()
            .twitch_for_channel_user(&target.channel, "alice", "ChannelFollow")
            .is_none());
    }

    #[tokio::test]
    async fn leaving_or_switching_accounts_cancels_follower_requests() {
        let index = index("alice");
        let target = follower_target(&index, "123");
        let mut requests = TwitchEmoteInventory::default();
        let (mut completion, pending) = tokio::sync::oneshot::channel();
        assert!(requests.start_follower_at(
            "alice",
            target.clone(),
            &index,
            Instant::now(),
            async { pending.await.unwrap() }
        ));
        index.write().unwrap().close_channel(&target.channel);
        requests.retain_followers(&index.read().unwrap().twitch_follower_targets());
        tokio::time::timeout(Duration::from_secs(3), completion.closed())
            .await
            .unwrap();
        assert!(!requests.has_pending());

        let target = follower_target(&index, "123");
        let (mut completion, pending) = tokio::sync::oneshot::channel();
        assert!(
            requests.start_follower_at("alice", target, &index, Instant::now(), async {
                pending.await.unwrap()
            })
        );
        index.write().unwrap().set_account(Some("bob"));
        assert!(requests.start_at("bob", &index, Instant::now(), async { Ok(Vec::new()) }));
        tokio::time::timeout(Duration::from_secs(3), completion.closed())
            .await
            .unwrap();
        assert!(requests.follower_requests.is_empty());
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
    async fn missing_scope_refreshes_globals_without_requesting_private_inventory() {
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
        assert!(requests.has_pending());
        assert!(requests.request.is_none());
        assert!(requests.follower_requests.is_empty());
        let generation = requests.global_request.as_ref().unwrap().generation;
        assert!(!requests.sync(Some("alice"), Some("client"), "token", &[], &index));
        assert_eq!(
            requests.global_request.as_ref().unwrap().generation,
            generation
        );
        requests.clear();
        let registry = index.read().unwrap();
        assert!(registry.twitch_for_user("alice", "SubLove").is_none());
        assert_eq!(
            registry.twitch_for_user("alice", "Kappa").unwrap().id,
            "global"
        );
    }

    #[tokio::test]
    async fn losing_inventory_scope_keeps_an_in_flight_global_catalog() {
        let index = index("alice");
        let mut requests = TwitchEmoteInventory::default();
        let now = Instant::now();
        assert!(requests.start_global_at("alice", &index, now, async {
            Ok(vec![info("global", "TwitchUnity")])
        }));
        let (mut completion, pending) = tokio::sync::oneshot::channel();
        assert!(requests.start_at("alice", &index, now, async { pending.await.unwrap() }));
        requests.sync(Some("alice"), Some("client"), "token", &[], &index);
        tokio::time::timeout(Duration::from_secs(3), completion.closed())
            .await
            .unwrap();
        assert!(requests.request.is_none());
        assert!(complete(&mut requests, &index).await);
        assert_eq!(index.read().unwrap().catalog()[0].code, "TwitchUnity");
    }
}

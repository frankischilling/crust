//! Helix reads for the live feed and reconnect state refreshes.

use async_trait::async_trait;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct FollowedChannel {
    pub broadcaster_id: String,
    pub broadcaster_login: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct HelixStream {
    pub user_id: String,
    pub user_login: String,
    pub user_name: String,
    /// Defaulted to 0 when missing - Twitch may omit for some stream types.
    #[serde(default)]
    pub viewer_count: u32,
    /// `None` when the field is absent from the API response.
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    /// `None` when the field is absent from the API response.
    #[serde(default)]
    pub started_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelixPollChoice {
    pub title: String,
    #[serde(default)]
    pub votes: u64,
    #[serde(default)]
    pub channel_points_votes: u64,
    #[serde(default)]
    pub bits_votes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelixPoll {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(default)]
    pub choices: Vec<HelixPollChoice>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelixPredictionOutcome {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub channel_points: u64,
    #[serde(default)]
    pub users: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelixPrediction {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(default)]
    pub outcomes: Vec<HelixPredictionOutcome>,
    pub winning_outcome_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelixHypeContribution {
    pub user_login: Option<String>,
    pub user_name: Option<String>,
    #[serde(rename = "type")]
    pub contribution_type: Option<String>,
    pub total: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelixHypeTrain {
    pub id: String,
    pub level: u32,
    pub total: u64,
    pub progress: u64,
    pub goal: u64,
    pub started_at: Option<String>,
    pub expires_at: Option<String>,
    pub cooldown_ends_at: Option<String>,
    #[serde(default)]
    pub top_contributions: Vec<HelixHypeContribution>,
    pub last_contribution: Option<HelixHypeContribution>,
}

#[derive(Debug, Deserialize)]
struct HelixHypeStatus {
    current: Option<HelixHypeTrain>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum HelixError {
    #[error("missing OAuth scope: {0}")]
    MissingScope(&'static str),
    #[error("not authenticated")]
    NotAuthenticated,
    #[error("rate limited by Helix")]
    RateLimited,
    #[error("http error: {0}")]
    Http(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("endpoint is unavailable: {0}")]
    Unsupported(&'static str),
}

/// Trait so the live-feed task can be tested without real HTTP.
#[async_trait]
pub trait HelixApi: Send + Sync {
    /// Returns ALL followed channels for `user_id`, auto-paginating across
    /// all pages internally. The caller does not drive pagination.
    async fn get_followed(&self, user_id: &str) -> Result<Vec<FollowedChannel>, HelixError>;

    /// Fetches live stream data for up to 100 `user_ids` in a single Helix
    /// call. Callers providing more than 100 IDs MUST chunk the slice
    /// themselves and call this multiple times.
    async fn get_streams(&self, user_ids: &[String]) -> Result<Vec<HelixStream>, HelixError>;

    async fn get_active_poll(
        &self,
        _broadcaster_id: &str,
    ) -> Result<Option<HelixPoll>, HelixError> {
        Err(HelixError::Unsupported("polls"))
    }

    async fn get_active_prediction(
        &self,
        _broadcaster_id: &str,
    ) -> Result<Option<HelixPrediction>, HelixError> {
        Err(HelixError::Unsupported("predictions"))
    }

    async fn get_active_hype_train(
        &self,
        _broadcaster_id: &str,
    ) -> Result<Option<HelixHypeTrain>, HelixError> {
        Err(HelixError::Unsupported("hypetrain/status"))
    }
}

use reqwest::header::AUTHORIZATION;

/// Real HTTP impl of `HelixApi`.
pub struct HelixClient {
    http: reqwest::Client,
    token: String,
    client_id: String,
    api_base: String,
}

impl HelixClient {
    pub fn new(http: reqwest::Client, token: String, client_id: String) -> Self {
        let token = token.trim();
        let token = token
            .strip_prefix("oauth:")
            .unwrap_or(token)
            .trim()
            .to_owned();
        Self {
            http,
            token,
            client_id,
            api_base: "https://api.twitch.tv/helix".to_owned(),
        }
    }

    pub(crate) fn streams_query(user_ids: &[String]) -> String {
        user_ids
            .iter()
            .map(|id| format!("user_id={id}"))
            .collect::<Vec<_>>()
            .join("&")
    }

    pub(crate) fn followed_url(user_id: &str, after: Option<&str>) -> String {
        let mut s =
            format!("https://api.twitch.tv/helix/channels/followed?user_id={user_id}&first=100");
        if let Some(cur) = after {
            s.push_str("&after=");
            s.push_str(cur);
        }
        s
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.token)
    }

    async fn get_page<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        query: &[(&str, &str)],
        scope: &'static str,
    ) -> Result<HelixPage<T>, HelixError> {
        let response = self
            .http
            .get(format!("{}/{endpoint}", self.api_base))
            .query(query)
            .header(AUTHORIZATION, self.auth_header())
            .header("Client-Id", &self.client_id)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| HelixError::Http(e.to_string()))?;
        Self::check_status(response.status(), scope)?;
        response
            .json()
            .await
            .map_err(|e| HelixError::Decode(e.to_string()))
    }

    /// Maps a Helix HTTP status to a `HelixError`. The `scope` argument is
    /// reported in `MissingScope` when the server returns 401 - pass the
    /// scope name the endpoint requires, or use `check_status_no_scope`
    /// for endpoints that don't gate on a particular scope.
    fn check_status(status: reqwest::StatusCode, scope: &'static str) -> Result<(), HelixError> {
        match status {
            reqwest::StatusCode::OK => Ok(()),
            reqwest::StatusCode::UNAUTHORIZED => Err(HelixError::MissingScope(scope)),
            reqwest::StatusCode::TOO_MANY_REQUESTS => Err(HelixError::RateLimited),
            s => Err(HelixError::Http(format!("status {s}"))),
        }
    }

    /// As `check_status` but reports a generic Http error on 401 instead of
    /// a `MissingScope`. Use for endpoints that don't gate on a specific scope.
    fn check_status_no_scope(status: reqwest::StatusCode) -> Result<(), HelixError> {
        match status {
            reqwest::StatusCode::OK => Ok(()),
            reqwest::StatusCode::UNAUTHORIZED => {
                Err(HelixError::Http("unauthorized (status 401)".to_owned()))
            }
            reqwest::StatusCode::TOO_MANY_REQUESTS => Err(HelixError::RateLimited),
            s => Err(HelixError::Http(format!("status {s}"))),
        }
    }
}

#[derive(Debug, Deserialize)]
struct HelixPage<T> {
    data: Vec<T>,
    #[serde(default)]
    pagination: Option<Pagination>,
}

#[derive(Debug, Deserialize)]
struct Pagination {
    #[serde(default)]
    cursor: Option<String>,
}

#[async_trait]
impl HelixApi for HelixClient {
    async fn get_followed(&self, user_id: &str) -> Result<Vec<FollowedChannel>, HelixError> {
        let mut out: Vec<FollowedChannel> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let url = Self::followed_url(user_id, cursor.as_deref());
            let resp = self
                .http
                .get(&url)
                .header(AUTHORIZATION, self.auth_header())
                .header("Client-Id", &self.client_id)
                .timeout(std::time::Duration::from_secs(15))
                .send()
                .await
                .map_err(|e| HelixError::Http(e.to_string()))?;
            Self::check_status(resp.status(), "user:read:follows")?;
            let page: HelixPage<FollowedChannel> = resp
                .json()
                .await
                .map_err(|e| HelixError::Decode(e.to_string()))?;
            out.extend(page.data);
            let next = page
                .pagination
                .and_then(|p| p.cursor)
                .filter(|c| !c.is_empty());
            match (cursor.as_ref(), next.as_ref()) {
                (Some(prev), Some(curr)) if prev == curr => {
                    // Helix returned the same non-empty cursor - treat as end of list.
                    break;
                }
                _ => {}
            }
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    async fn get_streams(&self, user_ids: &[String]) -> Result<Vec<HelixStream>, HelixError> {
        if user_ids.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!(
            "https://api.twitch.tv/helix/streams?{}&first=100",
            Self::streams_query(user_ids)
        );
        let resp = self
            .http
            .get(&url)
            .header(AUTHORIZATION, self.auth_header())
            .header("Client-Id", &self.client_id)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| HelixError::Http(e.to_string()))?;
        Self::check_status_no_scope(resp.status())?;
        let page: HelixPage<HelixStream> = resp
            .json()
            .await
            .map_err(|e| HelixError::Decode(e.to_string()))?;
        if page
            .pagination
            .as_ref()
            .and_then(|p| p.cursor.as_ref())
            .is_some_and(|c| !c.is_empty())
        {
            tracing::debug!(
                "helix: get_streams returned a pagination cursor for {} filtered ids; \
                 ignoring (Twitch does not normally paginate filtered /streams)",
                user_ids.len()
            );
        }
        Ok(page.data)
    }

    async fn get_active_poll(&self, broadcaster_id: &str) -> Result<Option<HelixPoll>, HelixError> {
        let page: HelixPage<HelixPoll> = self
            .get_page(
                "polls",
                &[("broadcaster_id", broadcaster_id), ("first", "1")],
                "channel:read:polls",
            )
            .await?;
        Ok(page
            .data
            .into_iter()
            .find(|poll| poll.status.eq_ignore_ascii_case("ACTIVE")))
    }

    async fn get_active_prediction(
        &self,
        broadcaster_id: &str,
    ) -> Result<Option<HelixPrediction>, HelixError> {
        let page: HelixPage<HelixPrediction> = self
            .get_page(
                "predictions",
                &[("broadcaster_id", broadcaster_id), ("first", "1")],
                "channel:read:predictions",
            )
            .await?;
        Ok(page.data.into_iter().find(|prediction| {
            prediction.status.eq_ignore_ascii_case("ACTIVE")
                || prediction.status.eq_ignore_ascii_case("LOCKED")
        }))
    }

    async fn get_active_hype_train(
        &self,
        broadcaster_id: &str,
    ) -> Result<Option<HelixHypeTrain>, HelixError> {
        let page: HelixPage<HelixHypeStatus> = self
            .get_page(
                "hypetrain/status",
                &[("broadcaster_id", broadcaster_id)],
                "channel:read:hype_train",
            )
            .await?;
        Ok(page.data.into_iter().find_map(|status| status.current))
    }
}

use std::sync::Arc;
use tokio::sync::RwLock as TokioRwLock;

/// Auth-swappable wrapper around `HelixClient`. The inner client is rebuilt
/// on each `set_auth` call so token / client_id changes propagate without
/// restarting the live-feed task.
pub struct AuthedHelix {
    inner: TokioRwLock<Option<Arc<HelixClient>>>,
    auth_changes: tokio::sync::watch::Sender<()>,
}

impl AuthedHelix {
    pub fn new() -> Self {
        Self {
            inner: TokioRwLock::new(None),
            auth_changes: tokio::sync::watch::channel(()).0,
        }
    }
    pub async fn set_auth(&self, token: String, client_id: String) {
        *self.inner.write().await = Some(Arc::new(HelixClient::new(
            reqwest::Client::new(),
            token,
            client_id,
        )));
        self.auth_changes.send_replace(());
    }
    pub async fn clear_auth(&self) {
        *self.inner.write().await = None;
        self.auth_changes.send_replace(());
    }

    /// Subscribe before starting account-scoped work so logout or a token
    /// change can cancel helpers that do not call through `HelixApi`.
    pub fn subscribe_auth_changes(&self) -> tokio::sync::watch::Receiver<()> {
        self.auth_changes.subscribe()
    }

    async fn snapshot(&self) -> Result<Arc<HelixClient>, HelixError> {
        self.inner
            .read()
            .await
            .clone()
            .ok_or(HelixError::NotAuthenticated)
    }

    async fn finish_request<T>(
        &self,
        client: &Arc<HelixClient>,
        result: Result<T, HelixError>,
    ) -> Result<T, HelixError> {
        if self
            .inner
            .read()
            .await
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, client))
        {
            result
        } else {
            // A request started by the previous account must not populate the UI.
            Err(HelixError::NotAuthenticated)
        }
    }
}

impl Default for AuthedHelix {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HelixApi for AuthedHelix {
    async fn get_followed(&self, user_id: &str) -> Result<Vec<FollowedChannel>, HelixError> {
        let client = self.snapshot().await?;
        let result = client.get_followed(user_id).await;
        self.finish_request(&client, result).await
    }
    async fn get_streams(&self, user_ids: &[String]) -> Result<Vec<HelixStream>, HelixError> {
        let client = self.snapshot().await?;
        let result = client.get_streams(user_ids).await;
        self.finish_request(&client, result).await
    }

    async fn get_active_poll(&self, broadcaster_id: &str) -> Result<Option<HelixPoll>, HelixError> {
        let client = self.snapshot().await?;
        let result = client.get_active_poll(broadcaster_id).await;
        self.finish_request(&client, result).await
    }

    async fn get_active_prediction(
        &self,
        broadcaster_id: &str,
    ) -> Result<Option<HelixPrediction>, HelixError> {
        let client = self.snapshot().await?;
        let result = client.get_active_prediction(broadcaster_id).await;
        self.finish_request(&client, result).await
    }

    async fn get_active_hype_train(
        &self,
        broadcaster_id: &str,
    ) -> Result<Option<HelixHypeTrain>, HelixError> {
        let client = self.snapshot().await?;
        let result = client.get_active_hype_train(broadcaster_id).await;
        self.finish_request(&client, result).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut request = Vec::new();
        while !request.windows(4).any(|part| part == b"\r\n\r\n") {
            let mut chunk = [0; 1024];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0, "connection closed before request headers");
            request.extend_from_slice(&chunk[..count]);
            assert!(request.len() <= 16 * 1024, "request headers too large");
        }
        String::from_utf8(request).unwrap()
    }

    async fn reply(stream: &mut tokio::net::TcpStream, body: &str) {
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
    }

    async fn fixture_client(body: &'static str) -> (HelixClient, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = HelixClient::new(
            reqwest::Client::new(),
            " oauth:test-token ".to_owned(),
            "test-client".to_owned(),
        );
        client.api_base = format!("http://{}/helix", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            reply(&mut stream, body).await;
            request
        });
        (client, task)
    }

    #[tokio::test]
    async fn backfill_poll_reads_current_state_and_encodes_query_parameters() {
        let (client, request) = fixture_client(r#"{"data":[{"id":"p1","title":"Choose","status":"ACTIVE","choices":[{"title":"A","votes":12}]}]}"#).await;
        let poll = client
            .get_active_poll("123&first=99")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(poll.id, "p1");
        assert_eq!(poll.choices[0].votes, 12);
        let request = request.await.unwrap();
        assert!(request.starts_with("GET /helix/polls?broadcaster_id=123%26first%3D99&first=1 "));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer test-token"));
        assert!(request
            .to_ascii_lowercase()
            .contains("client-id: test-client"));
    }

    #[tokio::test]
    async fn backfill_skips_finished_polls_and_keeps_locked_predictions() {
        let (client, request) = fixture_client(
            r#"{"data":[{"id":"p1","title":"Choose","status":"COMPLETED","choices":[]}]}"#,
        )
        .await;
        assert!(client.get_active_poll("123").await.unwrap().is_none());
        request.await.unwrap();
        let (client, request) = fixture_client(r#"{"data":[{"id":"p2","title":"Choose","status":"LOCKED","outcomes":[{"id":"o1","title":"A","channel_points":100,"users":2}]}]}"#).await;
        let prediction = client.get_active_prediction("123").await.unwrap().unwrap();
        assert_eq!(prediction.outcomes[0].channel_points, 100);
        assert!(request
            .await
            .unwrap()
            .starts_with("GET /helix/predictions?broadcaster_id=123&first=1 "));
    }

    #[tokio::test]
    async fn hype_train_status_uses_current_train_and_ignores_records() {
        let (client, request) = fixture_client(r#"{"data":[{"current":{"id":"train1","level":2,"total":700,"progress":200,"goal":1000,"expires_at":"2030-01-01T00:00:00Z","top_contributions":[{"user_login":"alice","type":"bits","total":50}]},"all_time_high":{"level":6,"total":2850}}]}"#).await;
        let train = client.get_active_hype_train("123").await.unwrap().unwrap();
        assert_eq!(train.id, "train1");
        assert_eq!(
            train.top_contributions[0].contribution_type.as_deref(),
            Some("bits")
        );
        assert!(request
            .await
            .unwrap()
            .starts_with("GET /helix/hypetrain/status?broadcaster_id=123 "));
        let (client, request) = fixture_client(
            r#"{"data":[{"current":null,"all_time_high":{"level":6,"total":2850}}]}"#,
        )
        .await;
        assert!(client.get_active_hype_train("123").await.unwrap().is_none());
        request.await.unwrap();
    }

    #[tokio::test]
    async fn logout_does_not_wait_for_http_and_discards_the_old_accounts_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = HelixClient::new(
            reqwest::Client::new(),
            "old-token".to_owned(),
            "client".to_owned(),
        );
        client.api_base = format!("http://{}/helix", listener.local_addr().unwrap());
        let helix = Arc::new(AuthedHelix::new());
        *helix.inner.write().await = Some(Arc::new(client));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            started_tx.send(()).unwrap();
            release_rx.await.unwrap();
            reply(
                &mut stream,
                r#"{"data":[{"id":"private-poll","title":"Private","status":"ACTIVE"}]}"#,
            )
            .await;
        });
        let requester = helix.clone();
        let request = tokio::spawn(async move { requester.get_active_poll("123").await });
        tokio::time::timeout(std::time::Duration::from_secs(3), started_rx)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), helix.clear_auth())
            .await
            .expect("logout is not blocked by a read lock");
        release_tx.send(()).unwrap();
        assert!(matches!(
            request.await.unwrap(),
            Err(HelixError::NotAuthenticated)
        ));
        server.await.unwrap();
    }

    #[test]
    fn followed_channel_decodes_from_helix_json() {
        let json = r#"{
            "broadcaster_id": "123",
            "broadcaster_login": "forsen",
            "broadcaster_name": "Forsen",
            "followed_at": "2020-01-01T00:00:00Z"
        }"#;
        let f: FollowedChannel = serde_json::from_str(json).unwrap();
        assert_eq!(f.broadcaster_id, "123");
        assert_eq!(f.broadcaster_login, "forsen");
    }

    #[test]
    fn helix_stream_decodes_from_helix_json() {
        let json = r#"{
            "id": "9",
            "user_id": "123",
            "user_login": "forsen",
            "user_name": "Forsen",
            "viewer_count": 5000,
            "thumbnail_url": "https://x/{width}x{height}.jpg",
            "started_at": "2026-04-22T10:00:00Z"
        }"#;
        let s: HelixStream = serde_json::from_str(json).unwrap();
        assert_eq!(s.user_login, "forsen");
        assert_eq!(s.viewer_count, 5000);
        assert_eq!(
            s.thumbnail_url.as_deref(),
            Some("https://x/{width}x{height}.jpg")
        );
        assert_eq!(s.started_at.as_deref(), Some("2026-04-22T10:00:00Z"));
    }

    #[test]
    fn helix_stream_decodes_with_missing_optional_fields() {
        // Some streams come back with missing viewer_count etc - treat as defaults.
        let json = r#"{
            "id": "9",
            "user_id": "123",
            "user_login": "forsen",
            "user_name": "Forsen"
        }"#;
        let s: HelixStream = serde_json::from_str(json).unwrap();
        assert_eq!(s.viewer_count, 0);
        assert!(
            s.thumbnail_url.is_none(),
            "missing thumbnail_url should be None, not Some(\"\")"
        );
        assert!(
            s.started_at.is_none(),
            "missing started_at should be None, not Some(\"\")"
        );
    }

    #[test]
    fn streams_query_string_includes_each_user_id() {
        let ids = vec!["1".to_owned(), "2".to_owned(), "3".to_owned()];
        let qs = HelixClient::streams_query(&ids);
        assert_eq!(qs, "user_id=1&user_id=2&user_id=3");
    }

    #[test]
    fn followed_url_includes_user_id_and_first() {
        let url = HelixClient::followed_url("999", None);
        assert_eq!(
            url,
            "https://api.twitch.tv/helix/channels/followed?user_id=999&first=100"
        );
    }

    #[test]
    fn followed_url_with_cursor_appends_after() {
        let url = HelixClient::followed_url("999", Some("cur1"));
        assert_eq!(
            url,
            "https://api.twitch.tv/helix/channels/followed?user_id=999&first=100&after=cur1"
        );
    }

    /// Sanity test for the helper that classifies HTTP statuses.
    #[test]
    fn check_status_maps_known_codes() {
        use reqwest::StatusCode;
        assert!(matches!(
            HelixClient::check_status(StatusCode::OK, "x"),
            Ok(())
        ));
        assert!(matches!(
            HelixClient::check_status(StatusCode::UNAUTHORIZED, "x"),
            Err(HelixError::MissingScope("x"))
        ));
        assert!(matches!(
            HelixClient::check_status(StatusCode::TOO_MANY_REQUESTS, "x"),
            Err(HelixError::RateLimited)
        ));
        assert!(matches!(
            HelixClient::check_status(StatusCode::INTERNAL_SERVER_ERROR, "x"),
            Err(HelixError::Http(_))
        ));
        assert!(matches!(
            HelixClient::check_status_no_scope(StatusCode::UNAUTHORIZED),
            Err(HelixError::Http(_))
        ));
    }

    #[tokio::test]
    async fn authed_helix_unauthenticated_returns_http_error() {
        let h = AuthedHelix::new();
        let err = h.get_followed("123").await.unwrap_err();
        assert!(matches!(err, HelixError::NotAuthenticated));
    }

    #[tokio::test]
    async fn authed_helix_clear_auth_returns_unauthenticated_again() {
        let h = AuthedHelix::new();
        h.set_auth("tok".into(), "cid".into()).await;
        h.clear_auth().await;
        let err = h.get_streams(&["1".into()]).await.unwrap_err();
        assert!(matches!(err, HelixError::NotAuthenticated));
    }
}

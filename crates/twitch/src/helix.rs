//! Minimal Helix API wrapper used by the Live feed.
//!
//! Only the two endpoints needed by `crates/app/src/runtime/live_feed.rs`
//! are implemented: `GET /helix/channels/followed` (paginated) and
//! `GET /helix/streams` (≤100 user_ids per call).

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
}

use reqwest::header::AUTHORIZATION;

/// Real HTTP impl of `HelixApi`.
pub struct HelixClient {
    http: reqwest::Client,
    token: String,
    client_id: String,
}

impl HelixClient {
    pub fn new(http: reqwest::Client, token: String, client_id: String) -> Self {
        Self {
            http,
            token,
            client_id,
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
}

use tokio::sync::RwLock as TokioRwLock;

/// Auth-swappable wrapper around `HelixClient`. The inner client is rebuilt
/// on each `set_auth` call so token / client_id changes propagate without
/// restarting the live-feed task.
pub struct AuthedHelix {
    inner: TokioRwLock<Option<HelixClient>>,
}

impl AuthedHelix {
    pub fn new() -> Self {
        Self {
            inner: TokioRwLock::new(None),
        }
    }
    pub async fn set_auth(&self, token: String, client_id: String) {
        *self.inner.write().await =
            Some(HelixClient::new(reqwest::Client::new(), token, client_id));
    }
    pub async fn clear_auth(&self) {
        *self.inner.write().await = None;
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
        let g = self.inner.read().await;
        match g.as_ref() {
            Some(c) => c.get_followed(user_id).await,
            None => Err(HelixError::NotAuthenticated),
        }
    }
    async fn get_streams(&self, user_ids: &[String]) -> Result<Vec<HelixStream>, HelixError> {
        let g = self.inner.read().await;
        match g.as_ref() {
            Some(c) => c.get_streams(user_ids).await,
            None => Err(HelixError::NotAuthenticated),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

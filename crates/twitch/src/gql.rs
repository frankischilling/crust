//! Viewer-facing reads from Twitch's web GraphQL endpoint.
//!
//! These queries use the web client's schema, which can change independently of
//! Helix. Missing fields produce errors so a schema change cannot reset a
//! balance or end a train.

use std::time::Duration;

use reqwest::{header, Client, StatusCode};
use serde_json::{json, Value};
use thiserror::Error;

const ENDPOINT: &str = "https://gql.twitch.tv/gql";
const WEB_CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

const POINTS_QUERY: &str = r#"
query CrustChannelPoints($login: String!) {
  currentUser { id }
  user(login: $login) {
    id
    channel {
      self { communityPoints { balance availableClaim { id } } }
      communityPointsSettings { isEnabled }
    }
  }
}
"#;

// Field names match the web client's GetHypeTrainExecution fragments:
// HypeTrainExecution -> HypeTrainProgress -> HypeTrainLevel.
const HYPE_QUERY: &str = r#"
query CrustHypeTrain($login: String!) {
  user(login: $login) {
    channel {
      hypeTrain {
        execution {
          id
          expiresAt
          endedAt
          progress { level { value } goal progression total }
          conductors { source participation { quantity } user { login } }
        }
      }
    }
  }
}
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelPointsContext {
    pub channel_id: String,
    pub balance: u64,
    pub available_claim_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelPointsMiss {
    NotAuthenticated,
    UserNotFound,
    NoPointsProgram,
    UnexpectedShape(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GqlHypeTrain {
    pub train_id: String,
    pub level: u32,
    pub progress: u64,
    pub goal: u64,
    pub total: u64,
    pub expires_at: Option<String>,
    pub ended_at: Option<String>,
    pub top_contributor_login: Option<String>,
    pub top_contributor_type: Option<String>,
    pub top_contributor_total: Option<u64>,
}

#[derive(Debug, Error)]
pub enum GqlError {
    #[error("GraphQL request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("GraphQL HTTP {0}")]
    Http(StatusCode),
    #[error("GraphQL response exceeds {MAX_RESPONSE_BYTES} bytes")]
    ResponseTooLarge,
    #[error("GraphQL response is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("GraphQL query failed: {0}")]
    Query(String),
    #[error("GraphQL response has a missing or invalid {0}")]
    Shape(&'static str),
    #[error("GraphQL channel login is empty or invalid")]
    InvalidLogin,
    #[error("GraphQL session token contains invalid header characters")]
    InvalidToken,
}

/// Read the signed-in viewer's balance. The token is the Twitch web session
/// token; an optional `oauth:` prefix is accepted.
pub async fn fetch_channel_points_context(
    http: &Client,
    login: &str,
    token: &str,
) -> Result<Result<ChannelPointsContext, ChannelPointsMiss>, GqlError> {
    fetch_points_at(http, ENDPOINT, login, token).await
}

/// Read the current public train without requiring a broadcaster access token.
pub async fn fetch_hype_train(
    http: &Client,
    login: &str,
) -> Result<Option<GqlHypeTrain>, GqlError> {
    fetch_hype_at(http, ENDPOINT, login).await
}

async fn fetch_points_at(
    http: &Client,
    endpoint: &str,
    login: &str,
    token: &str,
) -> Result<Result<ChannelPointsContext, ChannelPointsMiss>, GqlError> {
    let token = token.trim();
    let token = token.strip_prefix("oauth:").unwrap_or(token).trim();
    if token.is_empty() {
        return Ok(Err(ChannelPointsMiss::NotAuthenticated));
    }
    let data = request_data(http, endpoint, POINTS_QUERY, login, Some(token)).await?;
    Ok(parse_points(&data))
}

async fn fetch_hype_at(
    http: &Client,
    endpoint: &str,
    login: &str,
) -> Result<Option<GqlHypeTrain>, GqlError> {
    let data = request_data(http, endpoint, HYPE_QUERY, login, None).await?;
    parse_hype(&data)
}

async fn request_data(
    http: &Client,
    endpoint: &str,
    query: &str,
    login: &str,
    token: Option<&str>,
) -> Result<Value, GqlError> {
    let login = login.trim();
    if login.is_empty()
        || !login
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(GqlError::InvalidLogin);
    }
    let mut request = http
        .post(endpoint)
        .timeout(REQUEST_TIMEOUT)
        .header("Client-ID", WEB_CLIENT_ID)
        .header(header::CACHE_CONTROL, "no-cache")
        .json(&json!({
            "query": query,
            "variables": { "login": login.to_ascii_lowercase() }
        }));
    if let Some(token) = token {
        let mut authorization = header::HeaderValue::from_str(&format!("OAuth {token}"))
            .map_err(|_| GqlError::InvalidToken)?;
        authorization.set_sensitive(true);
        request = request.header(header::AUTHORIZATION, authorization);
    }
    let mut response = request.send().await?;
    if !response.status().is_success() {
        return Err(GqlError::Http(response.status()));
    }
    if response
        .content_length()
        .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
    {
        return Err(GqlError::ResponseTooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
            return Err(GqlError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    let mut envelope: Value = serde_json::from_slice(&body)?;
    if let Some(errors) = envelope.get("errors").filter(|value| !value.is_null()) {
        let errors = errors.as_array().ok_or(GqlError::Shape("errors"))?;
        if !errors.is_empty() {
            let message = errors
                .iter()
                .take(3)
                .map(|error| {
                    let text = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unspecified query error");
                    // Servers can include submitted values in validation errors.
                    let text = match token {
                        Some(token) => text.replace(token, "[redacted]"),
                        None => text.to_owned(),
                    };
                    text.chars().take(256).collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(GqlError::Query(message));
        }
    }
    let data = envelope
        .get_mut("data")
        .filter(|value| value.is_object())
        .ok_or(GqlError::Shape("data"))?;
    Ok(data.take())
}

fn field<'a>(value: &'a Value, key: &str, path: &'static str) -> Result<&'a Value, GqlError> {
    value.get(key).ok_or(GqlError::Shape(path))
}

fn text<'a>(value: &'a Value, path: &'static str) -> Result<&'a str, GqlError> {
    value
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or(GqlError::Shape(path))
}

fn number(value: &Value, path: &'static str) -> Result<u64, GqlError> {
    value.as_u64().ok_or(GqlError::Shape(path))
}

fn optional_text(value: &Value, path: &'static str) -> Result<Option<String>, GqlError> {
    if value.is_null() {
        Ok(None)
    } else {
        text(value, path).map(|text| Some(text.to_owned()))
    }
}

fn parse_points(data: &Value) -> Result<ChannelPointsContext, ChannelPointsMiss> {
    let parsed = (|| -> Result<Result<ChannelPointsContext, ChannelPointsMiss>, GqlError> {
        let user = field(data, "user", "user")?;
        if user.is_null() {
            return Ok(Err(ChannelPointsMiss::UserNotFound));
        }
        let current_user = field(data, "currentUser", "currentUser")?;
        if current_user.is_null() {
            return Ok(Err(ChannelPointsMiss::NotAuthenticated));
        }
        text(
            field(current_user, "id", "currentUser.id")?,
            "currentUser.id",
        )?;
        let channel_id = text(field(user, "id", "user.id")?, "user.id")?.to_owned();
        let channel = field(user, "channel", "user.channel")?;
        if channel.is_null() {
            return Ok(Err(ChannelPointsMiss::NoPointsProgram));
        }
        let settings = field(
            channel,
            "communityPointsSettings",
            "communityPointsSettings",
        )?;
        if settings.is_null() {
            return Ok(Err(ChannelPointsMiss::NoPointsProgram));
        }
        let enabled = field(settings, "isEnabled", "communityPointsSettings.isEnabled")?
            .as_bool()
            .ok_or(GqlError::Shape("communityPointsSettings.isEnabled"))?;
        if !enabled {
            return Ok(Err(ChannelPointsMiss::NoPointsProgram));
        }
        let viewer = field(channel, "self", "user.channel.self")?;
        if viewer.is_null() {
            return Ok(Err(ChannelPointsMiss::NotAuthenticated));
        }
        let points = field(viewer, "communityPoints", "communityPoints")?;
        if points.is_null() {
            return Ok(Err(ChannelPointsMiss::NoPointsProgram));
        }
        let balance = number(
            field(points, "balance", "communityPoints.balance")?,
            "communityPoints.balance",
        )?;
        let claim = field(points, "availableClaim", "communityPoints.availableClaim")?;
        let available_claim_id = if claim.is_null() {
            None
        } else {
            Some(
                text(
                    field(claim, "id", "availableClaim.id")?,
                    "availableClaim.id",
                )?
                .to_owned(),
            )
        };
        Ok(Ok(ChannelPointsContext {
            channel_id,
            balance,
            available_claim_id,
        }))
    })();
    match parsed {
        Ok(result) => result,
        Err(error) => Err(ChannelPointsMiss::UnexpectedShape(error.to_string())),
    }
}

fn parse_hype(data: &Value) -> Result<Option<GqlHypeTrain>, GqlError> {
    let user = field(data, "user", "user")?;
    if user.is_null() {
        return Ok(None);
    }
    let channel = field(user, "channel", "user.channel")?;
    if channel.is_null() {
        return Ok(None);
    }
    let hype = field(channel, "hypeTrain", "hypeTrain")?;
    if hype.is_null() {
        return Ok(None);
    }
    let train = field(hype, "execution", "hypeTrain.execution")?;
    if train.is_null() {
        return Ok(None);
    }
    let train_id = text(field(train, "id", "execution.id")?, "execution.id")?.to_owned();
    let progress = field(train, "progress", "execution.progress")?;
    let level = field(progress, "level", "progress.level")?;
    let level = number(
        field(level, "value", "progress.level.value")?,
        "progress.level.value",
    )?;
    let level = u32::try_from(level).map_err(|_| GqlError::Shape("progress.level.value"))?;
    let goal = number(field(progress, "goal", "progress.goal")?, "progress.goal")?;
    let total = number(
        field(progress, "total", "progress.total")?,
        "progress.total",
    )?;
    let progress = number(
        field(progress, "progression", "progress.progression")?,
        "progress.progression",
    )?;
    let expires_at = optional_text(
        field(train, "expiresAt", "execution.expiresAt")?,
        "execution.expiresAt",
    )?;
    let ended_at = optional_text(
        field(train, "endedAt", "execution.endedAt")?,
        "execution.endedAt",
    )?;
    let conductors = field(train, "conductors", "execution.conductors")?
        .as_array()
        .ok_or(GqlError::Shape("execution.conductors"))?;
    let conductor = conductors.iter().find(|conductor| {
        conductor
            .pointer("/user/login")
            .and_then(Value::as_str)
            .is_some_and(|login| !login.trim().is_empty())
    });
    let top_contributor_login = conductor
        .and_then(|conductor| conductor.pointer("/user/login"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let top_contributor_type = conductor
        .and_then(|conductor| conductor.get("source"))
        .and_then(Value::as_str)
        .map(|source| match source {
            "BITS" => "bits".to_owned(),
            "SUBS" => "subscription".to_owned(),
            source => source.to_ascii_lowercase(),
        });
    // Subscription quantities count subs, while the shared UI field expects
    // contribution points. Only Bits have the same unit in both APIs.
    let top_contributor_total = if top_contributor_type.as_deref() == Some("bits") {
        conductor
            .and_then(|conductor| conductor.pointer("/participation/quantity"))
            .and_then(Value::as_u64)
    } else {
        None
    };
    Ok(Some(GqlHypeTrain {
        train_id,
        level,
        progress,
        goal,
        total,
        expires_at,
        ended_at,
        top_contributor_login,
        top_contributor_type,
        top_contributor_total,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn points_data() -> Value {
        json!({
            "currentUser": {"id": "viewer-1"},
            "user": {
                "id": "channel-1",
                "channel": {
                    "self": {"communityPoints": {"balance": 0, "availableClaim": null}},
                    "communityPointsSettings": {"isEnabled": true}
                }
            }
        })
    }

    fn hype_data() -> Value {
        json!({"user": {"channel": {"hypeTrain": {"execution": {
            "id": "train-1",
            "expiresAt": "2026-11-14T00:05:00Z",
            "endedAt": null,
            "progress": {"level": {"value": 3}, "goal": 8000, "progression": 2500, "total": 13000},
            "conductors": [{
                "source": "BITS", "participation": {"quantity": 1500},
                "user": {"login": "alice"}
            }]
        }}}}})
    }

    async fn serve(response: String) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/gql", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(3), async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let n = socket.read(&mut buffer).await.unwrap();
                    assert_ne!(n, 0);
                    request.extend_from_slice(&buffer[..n]);
                    assert!(request.len() < 16 * 1024);
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                // Oversized responses can make the client close the socket early.
                let _ = socket.write_all(response.as_bytes()).await;
                String::from_utf8(request).unwrap()
            })
            .await
            .unwrap()
        });
        (endpoint, task)
    }

    fn response(body: &str) -> String {
        format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
    }

    fn client() -> Client {
        Client::builder().no_proxy().build().unwrap()
    }

    #[test]
    fn points_preserve_zero_balance_and_available_claim() {
        let mut data = points_data();
        let points = parse_points(&data).unwrap();
        assert_eq!(points.channel_id, "channel-1");
        assert_eq!(points.balance, 0);
        assert_eq!(points.available_claim_id, None);
        data["user"]["channel"]["self"]["communityPoints"]["availableClaim"] =
            json!({"id": "claim-1"});
        assert_eq!(
            parse_points(&data).unwrap().available_claim_id.as_deref(),
            Some("claim-1")
        );
    }

    #[test]
    fn points_distinguish_auth_absence_and_schema_changes() {
        let mut data = points_data();
        data["currentUser"] = Value::Null;
        assert_eq!(
            parse_points(&data),
            Err(ChannelPointsMiss::NotAuthenticated)
        );
        data["user"] = Value::Null;
        assert_eq!(parse_points(&data), Err(ChannelPointsMiss::UserNotFound));
        let mut data = points_data();
        data["user"]["channel"]["communityPointsSettings"]["isEnabled"] = json!(false);
        assert_eq!(parse_points(&data), Err(ChannelPointsMiss::NoPointsProgram));
        let mut data = points_data();
        data["user"]["channel"]["self"]["communityPoints"]
            .as_object_mut()
            .unwrap()
            .remove("balance");
        assert!(matches!(
            parse_points(&data),
            Err(ChannelPointsMiss::UnexpectedShape(_))
        ));
    }

    #[test]
    fn hype_uses_current_level_progress_and_preserves_end() {
        let mut data = hype_data();
        let train = parse_hype(&data).unwrap().unwrap();
        assert_eq!(
            (train.level, train.progress, train.goal, train.total),
            (3, 2500, 8000, 13000)
        );
        assert_eq!(train.top_contributor_login.as_deref(), Some("alice"));
        assert_eq!(train.top_contributor_type.as_deref(), Some("bits"));
        assert_eq!(train.top_contributor_total, Some(1500));
        assert_eq!(train.ended_at, None);
        let execution = &mut data["user"]["channel"]["hypeTrain"]["execution"];
        execution["endedAt"] = json!("2026-11-14T00:04:00Z");
        execution["conductors"][0]["source"] = json!("SUBS");
        execution["conductors"][0]["participation"]["quantity"] = json!(3);
        let train = parse_hype(&data).unwrap().unwrap();
        assert_eq!(train.ended_at.as_deref(), Some("2026-11-14T00:04:00Z"));
        assert_eq!(train.top_contributor_type.as_deref(), Some("subscription"));
        assert_eq!(train.top_contributor_total, None);
    }

    #[test]
    fn hype_does_not_turn_missing_or_invalid_fields_into_an_end() {
        let mut data = hype_data();
        data["user"]["channel"]["hypeTrain"]["execution"] = Value::Null;
        assert_eq!(parse_hype(&data).unwrap(), None);
        data["user"]["channel"]["hypeTrain"] = json!({});
        assert!(matches!(
            parse_hype(&data),
            Err(GqlError::Shape("hypeTrain.execution"))
        ));
        let mut data = hype_data();
        data["user"]["channel"]["hypeTrain"]["execution"]["progress"]["progression"] = json!(-1);
        assert!(matches!(
            parse_hype(&data),
            Err(GqlError::Shape("progress.progression"))
        ));
    }

    #[tokio::test]
    async fn points_request_normalizes_login_and_sends_sensitive_token_as_header() {
        let body = json!({"data": points_data()}).to_string();
        let (endpoint, server) = serve(response(&body)).await;
        let result = fetch_points_at(&client(), &endpoint, " Alice ", " oauth:secret-token ")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.balance, 0);
        let request = server.await.unwrap();
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(headers
            .to_ascii_lowercase()
            .contains("authorization: oauth secret-token"));
        assert!(headers
            .to_ascii_lowercase()
            .contains(&format!("client-id: {WEB_CLIENT_ID}")));
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["variables"]["login"], "alice");
        assert_eq!(body["query"], POINTS_QUERY);
        assert!(!body.to_string().contains("secret-token"));
    }

    #[tokio::test]
    async fn hype_request_is_anonymous_and_null_execution_is_successful_absence() {
        let body =
            json!({"data": {"user": {"channel": {"hypeTrain": {"execution": null}}}}}).to_string();
        let (endpoint, server) = serve(response(&body)).await;
        assert_eq!(
            fetch_hype_at(&client(), &endpoint, "alice").await.unwrap(),
            None
        );
        let request = server.await.unwrap();
        assert!(!request.to_ascii_lowercase().contains("authorization:"));
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["query"], HYPE_QUERY);
    }

    #[tokio::test]
    async fn http_and_query_failures_are_not_treated_as_empty_state() {
        let (endpoint, server) = serve(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_owned(),
        )
        .await;
        assert!(matches!(
            fetch_hype_at(&client(), &endpoint, "alice").await,
            Err(GqlError::Http(StatusCode::UNAUTHORIZED))
        ));
        server.await.unwrap();
        let body =
            json!({"data": points_data(), "errors": [{"message": "Unauthorized secret-token"}]})
                .to_string();
        let (endpoint, server) = serve(response(&body)).await;
        let error = fetch_points_at(&client(), &endpoint, "alice", "secret-token")
            .await
            .unwrap_err();
        assert!(matches!(error, GqlError::Query(_)));
        assert!(!error.to_string().contains("secret-token"));
        assert!(error.to_string().contains("Unauthorized [redacted]"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn response_limit_covers_declared_and_streamed_bodies() {
        let (endpoint, server) = serve(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_RESPONSE_BYTES + 1
        ))
        .await;
        assert!(matches!(
            fetch_hype_at(&client(), &endpoint, "alice").await,
            Err(GqlError::ResponseTooLarge)
        ));
        server.await.unwrap();
        let (endpoint, server) = serve(format!(
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{}",
            " ".repeat(MAX_RESPONSE_BYTES + 1)
        ))
        .await;
        assert!(matches!(
            fetch_hype_at(&client(), &endpoint, "alice").await,
            Err(GqlError::ResponseTooLarge)
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_envelopes_and_invalid_inputs_fail_explicitly() {
        for body in [
            "not JSON",
            "{}",
            r#"{"data":null}"#,
            r#"{"errors":{},"data":{}}"#,
        ] {
            let (endpoint, server) = serve(response(body)).await;
            assert!(fetch_hype_at(&client(), &endpoint, "alice").await.is_err());
            server.await.unwrap();
        }
        assert!(matches!(
            fetch_hype_at(&client(), "http://127.0.0.1:1", "bad login").await,
            Err(GqlError::InvalidLogin)
        ));
        assert!(matches!(
            fetch_points_at(&client(), "http://127.0.0.1:1", "alice", "bad\ntoken").await,
            Err(GqlError::InvalidToken)
        ));
        assert_eq!(
            fetch_points_at(&client(), "http://127.0.0.1:1", "alice", " oauth: ")
                .await
                .unwrap(),
            Err(ChannelPointsMiss::NotAuthenticated)
        );
    }
}

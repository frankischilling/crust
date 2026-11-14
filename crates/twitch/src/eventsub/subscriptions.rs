use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, warn};

use super::{is_expected_authz_failure, EventSubAuth, SubscriptionSpec};

type DesiredSubscriptions = BTreeMap<String, SubscriptionSpec>;

#[derive(Clone)]
struct SubscriptionState {
    session_id: String,
    specs: DesiredSubscriptions,
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CREATE_RETRY_DELAY: Duration = Duration::from_secs(30);
const DELETE_RETRY_DELAY: Duration = Duration::from_secs(5);

/// Owns one HTTP worker for an authenticated session, including socket transfers.
/// Desired-state updates replace older updates instead of queuing network work.
pub(super) struct SubscriptionWorker {
    desired: watch::Sender<SubscriptionState>,
    task: JoinHandle<()>,
}

impl SubscriptionWorker {
    pub(super) fn start(
        http: reqwest::Client,
        endpoint: String,
        auth: EventSubAuth,
        session_id: String,
        specs: Vec<SubscriptionSpec>,
        recover_existing: bool,
    ) -> Self {
        let (desired, receiver) = watch::channel(SubscriptionState {
            session_id: session_id.clone(),
            specs: spec_map(specs),
        });
        let task = tokio::spawn(reconcile(
            http,
            endpoint,
            auth,
            session_id,
            receiver,
            recover_existing,
        ));
        Self { desired, task }
    }

    pub(super) fn replace(&self, specs: Vec<SubscriptionSpec>) {
        let next = spec_map(specs);
        self.desired.send_if_modified(|desired| {
            if desired.specs == next {
                false
            } else {
                desired.specs = next;
                true
            }
        });
    }

    pub(super) fn resume(&self, session_id: String, specs: Vec<SubscriptionSpec>) {
        self.desired.send_replace(SubscriptionState {
            session_id,
            specs: spec_map(specs),
        });
    }
}

impl Drop for SubscriptionWorker {
    fn drop(&mut self) {
        // The old socket disables its subscriptions on disconnect. Aborting its
        // HTTP work prevents late responses from changing another session's state.
        self.task.abort();
    }
}

fn spec_key(kind: &str, version: &str, condition: &Value) -> String {
    format!("{kind}:{version}:{condition}")
}

fn spec_map(specs: Vec<SubscriptionSpec>) -> DesiredSubscriptions {
    specs
        .into_iter()
        .map(|spec| (spec_key(spec.kind, spec.version, &spec.condition), spec))
        .collect()
}

async fn reconcile(
    http: reqwest::Client,
    endpoint: String,
    auth: EventSubAuth,
    mut session_id: String,
    mut desired_rx: watch::Receiver<SubscriptionState>,
    mut recover_existing: bool,
) {
    let mut active: BTreeMap<String, String> = BTreeMap::new();
    let mut unavailable = HashSet::new();
    let mut create_retry: HashMap<String, Instant> = HashMap::new();
    let mut delete_retry: HashMap<String, Instant> = HashMap::new();

    loop {
        if desired_rx.has_changed().is_err() {
            return;
        }
        let state = desired_rx.borrow_and_update().clone();
        if state.session_id != session_id {
            session_id = state.session_id;
            recover_existing = true;
        }
        if recover_existing {
            // This runs only after the preceding POST/DELETE has completed. A
            // recovery GET must not overtake a mutation on the transferred IDs.
            match owned_subscriptions(&http, &endpoint, &auth, &session_id).await {
                Ok(found) => {
                    active = found;
                    unavailable.clear();
                    create_retry.clear();
                    delete_retry.clear();
                    recover_existing = false;
                    continue;
                }
                Err(error) => warn!("EventSub subscription recovery failed: {error}"),
            }
            tokio::select! {
                changed = desired_rx.changed() => if changed.is_err() { return; },
                _ = tokio::time::sleep(DELETE_RETRY_DELAY) => {}
            }
            continue;
        }
        let desired = state.specs;
        unavailable.retain(|key| desired.contains_key(key));
        create_retry.retain(|key, _| desired.contains_key(key) && !active.contains_key(key));
        delete_retry.retain(|key, _| active.contains_key(key) && !desired.contains_key(key));
        let now = Instant::now();

        // Cleanup takes priority over creating further subscriptions. A POST may
        // complete after an unwatch; its ID is retained here until DELETE succeeds.
        if let Some((key, id)) = active
            .iter()
            .find(|(key, _)| {
                !desired.contains_key(*key)
                    && delete_retry
                        .get(*key)
                        .is_none_or(|retry_at| now >= *retry_at)
            })
            .map(|(key, id)| (key.clone(), id.clone()))
        {
            match delete_subscription(&http, &endpoint, &auth, &id).await {
                Ok(()) => {
                    active.remove(&key);
                    delete_retry.remove(&key);
                }
                Err(error) => {
                    warn!("EventSub unsubscribe failed: {error}");
                    delete_retry.insert(key, Instant::now() + DELETE_RETRY_DELAY);
                }
            }
            continue;
        }

        if let Some((key, spec)) = desired
            .iter()
            .find(|(key, _)| {
                !active.contains_key(*key)
                    && !unavailable.contains(*key)
                    && create_retry
                        .get(*key)
                        .is_none_or(|retry_at| now >= *retry_at)
            })
            .map(|(key, spec)| (key.clone(), spec.clone()))
        {
            match create_subscription(&http, &endpoint, &auth, &session_id, &spec).await {
                Ok(Some(id)) => {
                    // Only the worker's private bookkeeping changes here. The next
                    // iteration reads the latest desired set before retaining the
                    // subscription or deleting an obsolete completion.
                    active.insert(key.clone(), id);
                    create_retry.remove(&key);
                }
                Ok(None) => {
                    create_retry.remove(&key);
                    unavailable.insert(key);
                }
                Err(error) => {
                    warn!("EventSub subscribe failed {}: {error}", spec.kind);
                    create_retry.insert(key, Instant::now() + CREATE_RETRY_DELAY);
                }
            }
            continue;
        }

        let next_retry = create_retry
            .values()
            .chain(delete_retry.values())
            .copied()
            .min();
        if let Some(next_retry) = next_retry {
            tokio::select! {
                changed = desired_rx.changed() => if changed.is_err() { return; },
                _ = tokio::time::sleep_until(next_retry) => {}
            }
        } else if desired_rx.changed().await.is_err() {
            return;
        }
    }
}

async fn create_subscription(
    http: &reqwest::Client,
    endpoint: &str,
    auth: &EventSubAuth,
    session_id: &str,
    spec: &SubscriptionSpec,
) -> Result<Option<String>, String> {
    let response = http
        .post(endpoint)
        .timeout(REQUEST_TIMEOUT)
        .bearer_auth(&auth.token)
        .header("Client-Id", &auth.client_id)
        .json(&json!({
            "type": spec.kind,
            "version": spec.version,
            "condition": spec.condition,
            "transport": { "method": "websocket", "session_id": session_id },
        }))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    if status == StatusCode::CONFLICT {
        let found = owned_subscriptions(http, endpoint, auth, session_id).await?;
        return Ok(found
            .get(&spec_key(spec.kind, spec.version, &spec.condition))
            .cloned());
    }
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        if is_expected_authz_failure(status, &body) {
            debug!(
                "EventSub subscription unavailable {}: HTTP {status} - {body}",
                spec.kind
            );
            return Ok(None);
        }
        return Err(format!("HTTP {status} - {body}"));
    }
    let response: Value = response.json().await.map_err(|error| error.to_string())?;
    response
        .get("data")
        .and_then(Value::as_array)
        .and_then(|data| data.first())
        .and_then(|subscription| subscription.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(|id| Some(id.to_owned()))
        .ok_or_else(|| "subscription response omitted its ID".to_owned())
}

async fn delete_subscription(
    http: &reqwest::Client,
    endpoint: &str,
    auth: &EventSubAuth,
    id: &str,
) -> Result<(), String> {
    let response = http
        .delete(endpoint)
        .timeout(REQUEST_TIMEOUT)
        .bearer_auth(&auth.token)
        .header("Client-Id", &auth.client_id)
        .query(&[("id", id)])
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if response.status().is_success() || response.status() == StatusCode::NOT_FOUND {
        Ok(())
    } else {
        Err(format!("HTTP {}", response.status()))
    }
}

async fn owned_subscriptions(
    http: &reqwest::Client,
    endpoint: &str,
    auth: &EventSubAuth,
    session_id: &str,
) -> Result<BTreeMap<String, String>, String> {
    let mut found = BTreeMap::new();
    let mut after = None;
    let mut cursors = HashSet::new();
    for _ in 0..100 {
        let mut request = http
            .get(endpoint)
            .timeout(REQUEST_TIMEOUT)
            .bearer_auth(&auth.token)
            .header("Client-Id", &auth.client_id)
            .query(&[("status", "enabled")]);
        if let Some(after) = after.as_deref() {
            request = request.query(&[("after", after)]);
        }
        let page: Value = request
            .send()
            .await
            .map_err(|error| error.to_string())?
            .error_for_status()
            .map_err(|error| error.to_string())?
            .json()
            .await
            .map_err(|error| error.to_string())?;
        if let Some(subscriptions) = page.get("data").and_then(Value::as_array) {
            for subscription in subscriptions {
                let transport = &subscription["transport"];
                if transport["method"].as_str() != Some("websocket")
                    || transport["session_id"].as_str() != Some(session_id)
                {
                    continue;
                }
                if let (Some(id), Some(kind), Some(version), Some(condition)) = (
                    subscription["id"].as_str(),
                    subscription["type"].as_str(),
                    subscription["version"].as_str(),
                    subscription.get("condition"),
                ) {
                    found.insert(spec_key(kind, version, condition), id.to_owned());
                }
            }
        }
        after = page["pagination"]["cursor"]
            .as_str()
            .filter(|cursor| !cursor.is_empty())
            .map(str::to_owned);
        let Some(cursor) = after.as_ref() else {
            return Ok(found);
        };
        if !cursors.insert(cursor.clone()) {
            return Err("subscription pagination repeated a cursor".to_owned());
        }
    }
    Err("subscription pagination exceeded its page limit".to_owned())
}

#[cfg(test)]
mod tests {
    use super::super::{
        EventSubCommand, EventSubConnectOutcome, EventSubEvent, EventSubSession,
        SubscriptionPriority, WatchedBroadcaster,
    };
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{mpsc, oneshot};
    use tokio_tungstenite::tungstenite::Message;

    fn auth() -> EventSubAuth {
        EventSubAuth {
            token: "test-token".to_owned(),
            client_id: "test-client".to_owned(),
            user_id: "moderator".to_owned(),
            scopes: HashSet::from(["moderator:read:followers".to_owned()]),
        }
    }

    fn follow_spec(broadcaster: &str) -> SubscriptionSpec {
        SubscriptionSpec {
            kind: "channel.follow",
            version: "2",
            condition: json!({"broadcaster_user_id": broadcaster, "moderator_user_id": "moderator"}),
            estimated_cost: 0,
            priority: SubscriptionPriority::Critical,
            fallback: None,
        }
    }

    async fn read_request(stream: &mut TcpStream) -> (String, Value) {
        let mut bytes = Vec::new();
        loop {
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if bytes.len() >= end + 4 + length {
                    let body = if length == 0 {
                        Value::Null
                    } else {
                        serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap()
                    };
                    return (headers.lines().next().unwrap().to_owned(), body);
                }
            }
            let mut chunk = [0; 2048];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count != 0, "request closed before its body arrived");
            bytes.extend_from_slice(&chunk[..count]);
        }
    }

    async fn respond(stream: &mut TcpStream, status: u16, body: Value) {
        let bytes = if body.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(&body).unwrap()
        };
        let headers = format!(
            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            bytes.len(),
        );
        stream.write_all(headers.as_bytes()).await.unwrap();
        stream.write_all(&bytes).await.unwrap();
        stream.shutdown().await.unwrap();
    }

    fn welcome() -> Message {
        Message::Text(
            json!({
                "metadata": {"message_type": "session_welcome"},
                "payload": {"session": {"id": "socket-test"}},
            })
            .to_string()
            .into(),
        )
    }

    fn session(
        endpoint: String,
    ) -> (
        EventSubSession,
        mpsc::Sender<EventSubCommand>,
        mpsc::Receiver<EventSubEvent>,
    ) {
        let (event_tx, event_rx) = mpsc::channel(16);
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let mut session = EventSubSession::new(event_tx, cmd_rx);
        session.auth = Some(auth());
        session.subscriptions_url = endpoint;
        session
            .watched_broadcasters
            .insert("123".to_owned(), WatchedBroadcaster { can_moderate: true });
        (session, cmd_tx, event_rx)
    }

    #[tokio::test]
    async fn logout_cancels_pending_subscription_http_without_waiting_for_response() {
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/subscriptions", http.local_addr().unwrap());
        let (started_tx, started_rx) = oneshot::channel();
        let http_server = tokio::spawn(async move {
            let (mut stream, _) = http.accept().await.unwrap();
            let (line, body) = read_request(&mut stream).await;
            assert!(line.starts_with("POST "));
            assert_eq!(body["transport"]["session_id"], "socket-test");
            started_tx.send(()).unwrap();
            let mut byte = [0];
            let read = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut byte))
                .await
                .unwrap();
            assert!(
                matches!(read, Ok(0) | Err(_)),
                "HTTP request survived logout"
            );
        });
        let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_url = format!("ws://{}", ws.local_addr().unwrap());
        let ws_server = tokio::spawn(async move {
            let (stream, _) = ws.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket.send(welcome()).await.unwrap();
            while let Some(Ok(_)) = socket.next().await {}
        });
        let (mut session, commands, _events) = session(endpoint);
        let client = tokio::spawn(async move { session.connect_once(&ws_url).await });
        tokio::time::timeout(Duration::from_secs(3), started_rx)
            .await
            .unwrap()
            .unwrap();
        commands.send(EventSubCommand::ClearAuth).await.unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(3), client)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(
            outcome,
            EventSubConnectOutcome::Reconnect {
                immediate: true,
                ..
            }
        ));
        http_server.await.unwrap();
        ws_server.await.unwrap();
    }

    #[tokio::test]
    async fn unwatch_deletes_remote_subscription_and_ignores_later_notifications() {
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/subscriptions", http.local_addr().unwrap());
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (deleted_tx, deleted_rx) = oneshot::channel();
        let http_server = tokio::spawn(async move {
            let (mut stream, _) = http.accept().await.unwrap();
            let (line, _) = read_request(&mut stream).await;
            assert!(line.starts_with("POST "));
            started_tx.send(()).unwrap();
            release_rx.await.unwrap();
            respond(&mut stream, 202, json!({"data": [{"id": "remote-123"}]})).await;
            let (mut stream, _) = http.accept().await.unwrap();
            let (line, _) = read_request(&mut stream).await;
            assert!(line.starts_with("DELETE /subscriptions?id=remote-123 "));
            respond(&mut stream, 204, Value::Null).await;
            deleted_tx.send(()).unwrap();
        });
        let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_url = format!("ws://{}", ws.local_addr().unwrap());
        let (processed_tx, processed_rx) = oneshot::channel();
        let ws_server = tokio::spawn(async move {
            let (stream, _) = ws.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket.send(welcome()).await.unwrap();
            deleted_rx.await.unwrap();
            socket.send(Message::Text(json!({
                "metadata": {"message_type": "notification", "subscription_type": "channel.follow", "message_id": "late"},
                "payload": {"event": {"broadcaster_user_id": "123", "broadcaster_user_login": "streamer", "user_login": "viewer"}},
            }).to_string().into())).await.unwrap();
            socket
                .send(Message::Ping(b"processed".to_vec().into()))
                .await
                .unwrap();
            let mut processed_tx = Some(processed_tx);
            while let Some(Ok(message)) = socket.next().await {
                if matches!(message, Message::Pong(_)) {
                    if let Some(tx) = processed_tx.take() {
                        let _ = tx.send(());
                    }
                }
            }
        });
        let (mut session, commands, mut events) = session(endpoint);
        let client = tokio::spawn(async move { session.connect_once(&ws_url).await });
        tokio::time::timeout(Duration::from_secs(3), started_rx)
            .await
            .unwrap()
            .unwrap();
        commands
            .send(EventSubCommand::UnwatchChannel {
                broadcaster_id: "123".to_owned(),
            })
            .await
            .unwrap();
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), processed_rx)
            .await
            .unwrap()
            .unwrap();
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event, EventSubEvent::Notice(_)),
                "notice arrived after unwatch"
            );
        }
        drop(commands);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), client)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            EventSubConnectOutcome::Stop
        ));
        http_server.await.unwrap();
        ws_server.await.unwrap();
    }

    #[tokio::test]
    async fn obsolete_post_completion_is_deleted_without_starting_stale_queued_work() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/subscriptions", listener.local_addr().unwrap());
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (line, body) = read_request(&mut stream).await;
            assert!(line.starts_with("POST "));
            assert_eq!(body["condition"]["broadcaster_user_id"], "123");
            started_tx.send(()).unwrap();
            release_rx.await.unwrap();
            respond(&mut stream, 202, json!({"data": [{"id": "obsolete"}]})).await;
            let (mut stream, _) = listener.accept().await.unwrap();
            let (line, _) = read_request(&mut stream).await;
            assert!(line.starts_with("DELETE /subscriptions?id=obsolete "));
            respond(&mut stream, 204, Value::Null).await;
        });
        let specs = vec![follow_spec("123"), follow_spec("456")];
        let worker = SubscriptionWorker::start(
            reqwest::Client::new(),
            endpoint,
            auth(),
            "socket-test".to_owned(),
            specs.clone(),
            false,
        );
        tokio::time::timeout(Duration::from_secs(3), started_rx)
            .await
            .unwrap()
            .unwrap();
        worker.replace(specs);
        worker.replace(Vec::new());
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        drop(worker);
    }

    #[tokio::test]
    async fn resume_waits_for_pending_delete_before_recovering_and_readding() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/subscriptions", listener.local_addr().unwrap());
        let (created_tx, created_rx) = oneshot::channel();
        let (deleting_tx, deleting_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (readded_tx, readded_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (line, body) = read_request(&mut stream).await;
            assert!(line.starts_with("POST "));
            assert_eq!(body["transport"]["session_id"], "old-socket");
            respond(&mut stream, 202, json!({"data": [{"id": "old-id"}]})).await;
            created_tx.send(()).unwrap();

            let (mut stream, _) = listener.accept().await.unwrap();
            let (line, _) = read_request(&mut stream).await;
            assert!(line.starts_with("DELETE /subscriptions?id=old-id "));
            deleting_tx.send(()).unwrap();
            tokio::select! {
                unexpected = listener.accept() => {
                    let (mut stream, _) = unexpected.unwrap();
                    let (line, _) = read_request(&mut stream).await;
                    panic!("request overtook the unfinished DELETE: {line}");
                }
                _ = release_rx => {}
            }
            respond(&mut stream, 204, Value::Null).await;

            let (mut stream, _) = listener.accept().await.unwrap();
            let (line, _) = read_request(&mut stream).await;
            assert!(line.starts_with("GET "));
            respond(&mut stream, 200, json!({"data": [], "pagination": {}})).await;

            let (mut stream, _) = listener.accept().await.unwrap();
            let (line, body) = read_request(&mut stream).await;
            assert!(line.starts_with("POST "));
            assert_eq!(body["transport"]["session_id"], "new-socket");
            respond(&mut stream, 202, json!({"data": [{"id": "new-id"}]})).await;
            readded_tx.send(()).unwrap();
        });
        let worker = SubscriptionWorker::start(
            reqwest::Client::new(),
            endpoint,
            auth(),
            "old-socket".to_owned(),
            vec![follow_spec("123")],
            false,
        );
        tokio::time::timeout(Duration::from_secs(3), created_rx)
            .await
            .unwrap()
            .unwrap();
        worker.replace(Vec::new());
        tokio::time::timeout(Duration::from_secs(3), deleting_rx)
            .await
            .unwrap()
            .unwrap();
        worker.resume("new-socket".to_owned(), vec![follow_spec("123")]);
        tokio::time::sleep(Duration::from_millis(25)).await;
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), readded_rx)
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
        drop(worker);
    }

    #[tokio::test]
    async fn resume_keeps_old_socket_until_welcome_and_reuses_transferred_ids() {
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/subscriptions", http.local_addr().unwrap());
        let (created_tx, created_rx) = oneshot::channel();
        let (recovered_tx, recovered_rx) = oneshot::channel();
        let (deleted_tx, deleted_rx) = oneshot::channel();
        let http_server = tokio::spawn(async move {
            let (mut stream, _) = http.accept().await.unwrap();
            let (line, body) = read_request(&mut stream).await;
            assert!(line.starts_with("POST "));
            assert_eq!(body["transport"]["session_id"], "old-socket");
            respond(&mut stream, 202, json!({"data": [{"id": "transferred"}]})).await;
            created_tx.send(()).unwrap();

            let (mut stream, _) = http.accept().await.unwrap();
            let (line, _) = read_request(&mut stream).await;
            assert!(
                line.starts_with("GET "),
                "resume duplicated a subscription: {line}"
            );
            respond(
                &mut stream,
                200,
                json!({"data": [{
                "id": "transferred", "type": "channel.follow", "version": "2",
                "condition": {"broadcaster_user_id": "123", "moderator_user_id": "moderator"},
                "transport": {"method": "websocket", "session_id": "new-socket"}
            }], "pagination": {}}),
            )
            .await;
            recovered_tx.send(()).unwrap();

            let (mut stream, _) = http.accept().await.unwrap();
            let (line, _) = read_request(&mut stream).await;
            assert!(line.starts_with("DELETE /subscriptions?id=transferred "));
            respond(&mut stream, 204, Value::Null).await;
            deleted_tx.send(()).unwrap();
        });

        let new_ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let reconnect_url = format!("ws://{}", new_ws.local_addr().unwrap());
        let (new_connected_tx, new_connected_rx) = oneshot::channel();
        let (allow_welcome_tx, allow_welcome_rx) = oneshot::channel();
        let new_server = tokio::spawn(async move {
            let (stream, _) = new_ws.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            new_connected_tx.send(()).unwrap();
            allow_welcome_rx.await.unwrap();
            socket
                .send(Message::Text(
                    json!({
                        "metadata": {"message_type": "session_welcome"},
                        "payload": {"session": {"id": "new-socket"}}
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            while let Some(Ok(_)) = socket.next().await {}
        });

        let old_ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_url = format!("ws://{}", old_ws.local_addr().unwrap());
        let (old_pong_tx, old_pong_rx) = oneshot::channel();
        let (old_closed_tx, mut old_closed_rx) = oneshot::channel();
        let old_server = tokio::spawn(async move {
            let (stream, _) = old_ws.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket
                .send(Message::Text(
                    json!({
                        "metadata": {"message_type": "session_welcome"},
                        "payload": {"session": {"id": "old-socket"}}
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            created_rx.await.unwrap();
            socket
                .send(Message::Text(
                    json!({
                        "metadata": {"message_type": "session_reconnect"},
                        "payload": {"session": {"reconnect_url": reconnect_url}}
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            socket
                .send(Message::Ping(b"old-active".to_vec().into()))
                .await
                .unwrap();
            let mut old_pong_tx = Some(old_pong_tx);
            while let Some(Ok(message)) = socket.next().await {
                if matches!(message, Message::Pong(_)) {
                    if let Some(tx) = old_pong_tx.take() {
                        let _ = tx.send(());
                    }
                }
            }
            old_closed_tx.send(()).unwrap();
        });

        let (mut session, commands, mut events) = session(endpoint);
        let client = tokio::spawn(async move { session.connect_once(&ws_url).await });
        tokio::time::timeout(Duration::from_secs(3), new_connected_rx)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), old_pong_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            old_closed_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        allow_welcome_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), recovered_rx)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), old_closed_rx)
            .await
            .unwrap()
            .unwrap();
        commands
            .send(EventSubCommand::UnwatchChannel {
                broadcaster_id: "123".to_owned(),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), deleted_rx)
            .await
            .unwrap()
            .unwrap();
        let mut connection_states = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let EventSubEvent::Connected { resumed } = event {
                connection_states.push(resumed);
            }
        }
        assert_eq!(connection_states, [false, true]);
        drop(commands);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), client)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            EventSubConnectOutcome::Stop
        ));
        http_server.await.unwrap();
        old_server.await.unwrap();
        new_server.await.unwrap();
    }

    #[tokio::test]
    async fn recovered_ids_are_limited_to_the_current_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/subscriptions", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (line, _) = read_request(&mut stream).await;
            assert!(line.starts_with("GET /subscriptions?status=enabled "));
            respond(&mut stream, 200, json!({"data": [
                {"id": "ours", "type": "channel.follow", "version": "2", "condition": {"broadcaster_user_id": "123", "moderator_user_id": "moderator"}, "transport": {"method": "websocket", "session_id": "socket-test"}},
                {"id": "foreign", "type": "channel.follow", "version": "2", "condition": {"broadcaster_user_id": "456", "moderator_user_id": "moderator"}, "transport": {"method": "websocket", "session_id": "another-socket"}}
            ], "pagination": {}})).await;
        });
        let found = owned_subscriptions(&reqwest::Client::new(), &endpoint, &auth(), "socket-test")
            .await
            .unwrap();
        assert_eq!(
            found.values().map(String::as_str).collect::<Vec<_>>(),
            ["ours"]
        );
        server.await.unwrap();
    }
}

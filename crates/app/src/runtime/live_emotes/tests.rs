use super::*;
use std::future::Future;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

type ServerSocket = WebSocketStream<TcpStream>;

async fn within<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("loopback operation timed out")
}

fn emote(provider: Provider, id: &str, code: &str) -> EmoteInfo {
    match provider {
        Provider::Bttv => bttv::parse_emote(&json!({ "id": id, "code": code })).unwrap(),
        Provider::SevenTv => {
            crust_emotes::providers::parse_7tv_emote(active_emote(id, code)).unwrap()
        }
    }
}

fn active_emote(id: &str, code: &str) -> Value {
    json!({ "id": id, "name": code, "data": { "host": {
        "url": format!("//cdn.7tv.app/emote/{id}"),
        "files": [{ "name": "1x.webp", "width": 32 }]
    }}})
}

struct Harness {
    index: crate::EmoteIndex,
    channel: ChannelId,
    desired: watch::Sender<Vec<LiveTarget>>,
    dirty: mpsc::Receiver<()>,
    websocket: TcpListener,
    http: TcpListener,
    socket: ServerSocket,
    task: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn new(provider: Provider, emotes: Vec<EmoteInfo>) -> Self {
        let index = crate::EmoteIndex::default();
        let channel = ChannelId::new("loopback");
        let source = LiveSource {
            provider,
            room_id: "42".into(),
            platform: "TWITCH".into(),
            user_id: (provider == Provider::SevenTv).then(|| "user-id".into()),
            set_id: (provider == Provider::SevenTv).then(|| "set-id".into()),
            connection_index: (provider == Provider::SevenTv).then_some(0),
        };
        {
            let mut registry = index.write().unwrap();
            registry.open_channel(&channel);
            let generation = registry.begin_channel_load(&channel, "42", false).unwrap();
            assert!(registry.replace_channel_with_source(
                &channel,
                generation,
                provider.name(),
                Some(emotes),
                Some(source),
                None
            ));
            assert!(registry.complete_channel_load(&channel, generation));
        }
        let websocket = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoints = Endpoints {
            socket: format!("ws://{}/ws", websocket.local_addr().unwrap()),
            bttv_api: format!("http://{}/bttv", http.local_addr().unwrap()),
            seven_tv_gql: format!("http://{}/gql", http.local_addr().unwrap()),
        };
        let (desired, updates) = watch::channel(index.read().unwrap().live_targets());
        let (changed, dirty) = mpsc::channel(1);
        let task = tokio::spawn(run_worker(
            provider,
            endpoints,
            index.clone(),
            changed,
            updates,
        ));
        let (stream, _) = within(websocket.accept()).await.unwrap();
        let socket = within(tokio_tungstenite::accept_async(stream))
            .await
            .unwrap();
        Self {
            index,
            channel,
            desired,
            dirty,
            websocket,
            http,
            socket,
            task,
        }
    }

    fn has(&self, code: &str) -> bool {
        self.index
            .read()
            .unwrap()
            .resolve(&self.channel, "", code)
            .is_some()
    }

    async fn wait_for(&mut self, code: &str) {
        within(async {
            while !self.has(code) {
                self.dirty
                    .recv()
                    .await
                    .expect("worker stopped before publishing its snapshot");
            }
        })
        .await;
    }

    async fn reconnect(&mut self) {
        assert!(
            matches!(
                within(self.socket.next()).await,
                None | Some(Err(_)) | Some(Ok(Message::Close(_)))
            ),
            "a changed binding must close its old socket"
        );
        let (stream, _) = within(self.websocket.accept()).await.unwrap();
        self.socket = within(tokio_tungstenite::accept_async(stream))
            .await
            .unwrap();
    }

    async fn request(&self) -> (TcpStream, String) {
        within(async {
            let (mut stream, _) = self.http.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 1024];
            loop {
                let received = stream.read(&mut chunk).await.unwrap();
                assert!(
                    received > 0,
                    "client closed before completing its HTTP request"
                );
                bytes.extend_from_slice(&chunk[..received]);
                assert!(
                    bytes.len() <= 64 * 1024,
                    "unexpectedly large fixture request"
                );
                if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&bytes[..end]).unwrap();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            (stream, String::from_utf8(bytes).unwrap())
        })
        .await
    }

    async fn assert_no_request(&self) {
        assert!(
            tokio::time::timeout(Duration::from_millis(40), self.http.accept())
                .await
                .is_err(),
            "a snapshot started before every subscription was acknowledged"
        );
    }

    async fn stop(self) {
        drop(self.desired);
        within(self.task).await.unwrap();
    }
}

async fn send(socket: &mut ServerSocket, value: Value) {
    within(socket.send(Message::Text(value.to_string().into())))
        .await
        .unwrap();
}

async fn next_text(socket: &mut ServerSocket) -> Value {
    within(async {
        loop {
            match socket.next().await.expect("websocket closed").unwrap() {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(payload) => socket.send(Message::Pong(payload)).await.unwrap(),
                Message::Pong(_) => {}
                other => panic!("expected websocket command, received {other:?}"),
            }
        }
    })
    .await
}

/// A pong proves that the worker has reduced all earlier frames on this socket.
async fn barrier(socket: &mut ServerSocket, marker: &[u8]) {
    within(socket.send(Message::Ping(marker.to_vec().into())))
        .await
        .unwrap();
    within(async {
        loop {
            match socket
                .next()
                .await
                .expect("websocket closed before barrier")
                .unwrap()
            {
                Message::Pong(payload) if &payload[..] == marker => return,
                Message::Pong(_) => {}
                Message::Ping(payload) => socket.send(Message::Pong(payload)).await.unwrap(),
                other => panic!("unexpected frame before barrier: {other:?}"),
            }
        }
    })
    .await;
}

async fn respond(stream: &mut TcpStream, body: Value) {
    let body = body.to_string();
    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    within(stream.write_all(response.as_bytes())).await.unwrap();
    within(stream.shutdown()).await.unwrap();
}

async fn acknowledge(socket: &mut ServerSocket, command: &Value) {
    assert_eq!(command["op"], 35);
    send(
        socket,
        json!({ "op": 5, "d": { "command": "SUBSCRIBE", "data": command["d"] } }),
    )
    .await;
}

fn graphql_snapshot(set: &str, emotes: Vec<Value>) -> Value {
    json!({ "data": {
        "userByConnection": { "id": "user-id", "connections": [
            { "id": "42", "platform": "TWITCH", "emote_set_id": set }
        ]},
        "sets": [{ "id": set, "emotes": emotes }]
    }})
}

#[tokio::test]
async fn bttv_subscribes_and_reconciles_live_deletes_with_a_delayed_cached_snapshot() {
    let mut fixture = Harness::new(
        Provider::Bttv,
        vec![
            emote(Provider::Bttv, "gone", "Gone"),
            emote(Provider::Bttv, "keep", "Keep"),
        ],
    )
    .await;
    assert_eq!(
        next_text(&mut fixture.socket).await,
        json!({ "name": "join_channel", "data": { "name": "twitch:42" } })
    );
    // Hold the HTTP response after the socket has joined the room.
    let (mut request, headers) = fixture.request().await;
    assert!(headers.starts_with("GET /bttv/42 "));

    send(
        &mut fixture.socket,
        json!({ "name": "emote_delete", "data": {
            "channel": "twitch:999", "emoteId": "keep"
        }}),
    )
    .await;
    barrier(&mut fixture.socket, b"foreign-room").await;
    assert!(fixture.has("Keep"));
    assert!(fixture.has("Gone"));

    send(
        &mut fixture.socket,
        json!({ "name": "emote_delete", "data": {
            "channel": "twitch:42", "emoteId": "gone"
        }}),
    )
    .await;
    barrier(&mut fixture.socket, b"live-delete").await;
    assert!(!fixture.has("Gone"));
    respond(
        &mut request,
        json!({ "channelEmotes": [
        { "id": "gone", "code": "Gone" }, { "id": "keep", "code": "Keep" },
        { "id": "rest", "code": "RestMarker" }
    ], "sharedEmotes": [] }),
    )
    .await;
    fixture.wait_for("RestMarker").await;
    assert!(
        !fixture.has("Gone"),
        "the cached response resurrected a live removal"
    );
    assert!(fixture.has("Keep"));
    fixture.stop().await;
}

#[tokio::test]
async fn seven_tv_waits_for_acks_and_preserves_updates_across_snapshot_and_set_switch() {
    let mut fixture = Harness::new(
        Provider::SevenTv,
        vec![emote(Provider::SevenTv, "old", "Old")],
    )
    .await;
    barrier(&mut fixture.socket, b"before-hello").await;
    fixture.assert_no_request().await;
    send(
        &mut fixture.socket,
        json!({ "op": 1, "d": {
            "heartbeat_interval": 60000, "subscription_limit": 100
        }}),
    )
    .await;
    let mut subscriptions = Vec::new();
    for _ in 0..3 {
        subscriptions.push(next_text(&mut fixture.socket).await);
    }
    let topics: HashSet<_> = subscriptions
        .iter()
        .map(|command| {
            assert_eq!(command["op"], 35);
            (
                command["d"]["type"].as_str().unwrap(),
                command["d"]["condition"]["object_id"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        topics,
        HashSet::from([
            ("user.update", "user-id"),
            ("emote_set.update", "set-id"),
            ("emote_set.delete", "set-id"),
        ])
    );
    for subscription in &subscriptions[..2] {
        acknowledge(&mut fixture.socket, subscription).await;
    }
    barrier(&mut fixture.socket, b"partial-acks").await;
    fixture.assert_no_request().await;
    acknowledge(&mut fixture.socket, &subscriptions[2]).await;
    let (mut request, headers) = fixture.request().await;
    assert!(headers.starts_with("POST /gql "));
    let request_body: Value =
        serde_json::from_str(headers.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(request_body["variables"]["room"], "42");
    assert_eq!(request_body["variables"]["set"], "set-id");

    send(
        &mut fixture.socket,
        json!({ "op": 0, "d": { "type": "emote_set.update", "body": {
            "id": "set-id", "pushed": [{ "key": "emotes", "value": active_emote("live", "Live") }]
        }}}),
    )
    .await;
    barrier(&mut fixture.socket, b"post-request-update").await;
    assert!(fixture.has("Live"));
    respond(
        &mut request,
        graphql_snapshot(
            "set-id",
            vec![
                active_emote("old", "Old"),
                active_emote("rest", "RestMarker"),
            ],
        ),
    )
    .await;
    fixture.wait_for("RestMarker").await;
    assert!(
        fixture.has("Live"),
        "the in-flight snapshot overwrote a newer live addition"
    );

    send(
        &mut fixture.socket,
        json!({ "op": 0, "d": { "type": "user.update", "body": {
            "id": "user-id", "updated": [{ "key": "connections", "index": 0,
                "value": [{ "key": "emote_set_id", "value": "new-set" }] }]
        }}}),
    )
    .await;
    barrier(&mut fixture.socket, b"active-set-switch").await;
    assert!(!fixture.has("Old"));
    assert!(!fixture.has("Live"));
    let targets = fixture.index.read().unwrap().live_targets();
    assert_eq!(targets[0].source.set_id.as_deref(), Some("new-set"));
    send(
        &mut fixture.socket,
        json!({ "op": 0, "d": { "type": "emote_set.update", "body": {
            "id": "set-id", "pushed": [{ "key": "emotes", "value": active_emote("stale", "Stale") }]
        }}}),
    )
    .await;
    barrier(&mut fixture.socket, b"old-set-after-switch").await;
    assert!(!fixture.has("Stale"));
    fixture.desired.send(targets).unwrap();
    fixture.reconnect().await;
    barrier(&mut fixture.socket, b"new-binding-before-hello").await;
    fixture.assert_no_request().await;
    send(
        &mut fixture.socket,
        json!({ "op": 1, "d": {
            "heartbeat_interval": 60000, "subscription_limit": 100
        }}),
    )
    .await;
    let mut new_subscriptions = Vec::new();
    for _ in 0..3 {
        new_subscriptions.push(next_text(&mut fixture.socket).await);
    }
    let topics: HashSet<_> = new_subscriptions
        .iter()
        .map(|command| {
            assert_eq!(command["op"], 35);
            (
                command["d"]["type"].as_str().unwrap(),
                command["d"]["condition"]["object_id"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        topics,
        HashSet::from([
            ("user.update", "user-id"),
            ("emote_set.update", "new-set"),
            ("emote_set.delete", "new-set"),
        ])
    );
    for subscription in &new_subscriptions[..2] {
        acknowledge(&mut fixture.socket, subscription).await;
    }
    barrier(&mut fixture.socket, b"new-binding-partial-acks").await;
    fixture.assert_no_request().await;
    acknowledge(&mut fixture.socket, &new_subscriptions[2]).await;
    let (mut request, headers) = fixture.request().await;
    let request_body: Value =
        serde_json::from_str(headers.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(request_body["variables"]["set"], "new-set");
    respond(
        &mut request,
        graphql_snapshot("new-set", vec![active_emote("new", "NewSet")]),
    )
    .await;
    fixture.wait_for("NewSet").await;
    assert!(!fixture.has("Live"));
    assert!(!fixture.has("Stale"));
    fixture.stop().await;
}

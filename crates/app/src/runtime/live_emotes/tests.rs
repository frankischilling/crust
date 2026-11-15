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
    scope: LiveScope,
    desired: watch::Sender<Vec<LiveTarget>>,
    dirty: mpsc::Receiver<()>,
    cosmetics: mpsc::Receiver<SevenTvCosmeticUpdate>,
    websocket: TcpListener,
    http: TcpListener,
    socket: ServerSocket,
    task: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn new(provider: Provider, emotes: Vec<EmoteInfo>) -> Self {
        Self::new_scoped(
            provider,
            emotes,
            LiveScope::Channel(ChannelId::new("loopback")),
        )
        .await
    }

    async fn new_scoped(provider: Provider, emotes: Vec<EmoteInfo>, scope: LiveScope) -> Self {
        let index = crate::EmoteIndex::default();
        let channel = ChannelId::new("loopback");
        let global = matches!(scope, LiveScope::Global);
        let source = LiveSource {
            provider,
            room_id: if global { "global" } else { "42" }.into(),
            platform: if global { "GLOBAL" } else { "TWITCH" }.into(),
            user_id: (provider == Provider::SevenTv && !global).then(|| "user-id".into()),
            set_id: (provider == Provider::SevenTv).then(|| "set-id".into()),
            connection_index: (provider == Provider::SevenTv
                && matches!(scope, LiveScope::Channel(_)))
            .then_some(0),
        };
        {
            let mut registry = index.write().unwrap();
            match &scope {
                LiveScope::Channel(channel) => {
                    registry.open_channel(channel);
                    let generation = registry.begin_channel_load(channel, "42", false).unwrap();
                    assert!(registry.replace_channel_with_source(
                        channel,
                        generation,
                        provider.name(),
                        Some(emotes),
                        Some(source),
                        None,
                    ));
                    assert!(registry.complete_channel_load(channel, generation));
                }
                LiveScope::Global => {
                    registry.replace_global_live(source, emotes, None);
                }
                LiveScope::Personal(user) => {
                    registry.set_account(Some(user));
                    let generation = registry.begin_personal_load(user, None).unwrap();
                    registry
                        .complete_personal_load_with_source(
                            user,
                            generation,
                            Some(emotes),
                            Some(source),
                        )
                        .unwrap();
                }
            }
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
        let (cosmetic_tx, cosmetics) = mpsc::channel(512);
        let task = tokio::spawn(run_worker(
            provider,
            endpoints,
            index.clone(),
            changed,
            updates,
            Some(cosmetic_tx),
        ));
        let (stream, _) = within(websocket.accept()).await.unwrap();
        let socket = within(tokio_tungstenite::accept_async(stream))
            .await
            .unwrap();
        Self {
            index,
            channel,
            scope,
            desired,
            dirty,
            cosmetics,
            websocket,
            http,
            socket,
            task,
        }
    }

    fn has(&self, code: &str) -> bool {
        let user = match &self.scope {
            LiveScope::Personal(user) => user.as_str(),
            _ => "",
        };
        self.index
            .read()
            .unwrap()
            .resolve(&self.channel, user, code)
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

fn scoped_snapshot(scope: &LiveScope, set: &str, emotes: Vec<Value>) -> Value {
    match scope {
        LiveScope::Channel(_) => graphql_snapshot(set, emotes),
        LiveScope::Global => json!({"data": {"sets": [{"id": set, "emotes": emotes}]}}),
        LiveScope::Personal(_) => json!({"data": {
            "userByConnection": {"id": "user-id", "emote_sets": [{"id": set, "flags": 4}]},
            "sets": [{"id": set, "emotes": emotes}]
        }}),
    }
}

async fn ready_scoped(fixture: &mut Harness) {
    let count = match fixture.scope {
        LiveScope::Global => 2,
        LiveScope::Personal(_) => 3,
        LiveScope::Channel(_) => 6,
    };
    send(
        &mut fixture.socket,
        json!({"op": 1, "d": {"heartbeat_interval": 60000, "subscription_limit": 100}}),
    )
    .await;
    let mut subscriptions = Vec::new();
    for _ in 0..count {
        subscriptions.push(next_text(&mut fixture.socket).await);
    }
    for subscription in &subscriptions[..count - 1] {
        acknowledge(&mut fixture.socket, subscription).await;
    }
    barrier(&mut fixture.socket, b"scoped-partial-acks").await;
    fixture.assert_no_request().await;
    acknowledge(&mut fixture.socket, &subscriptions[count - 1]).await;
}

#[tokio::test]
async fn seven_tv_overlay_updates_survive_snapshots_and_stay_in_their_scope() {
    for scope in [
        LiveScope::Global,
        LiveScope::Personal("42".into()),
        LiveScope::Channel(ChannelId::new("loopback")),
    ] {
        let mut fixture = Harness::new_scoped(
            Provider::SevenTv,
            vec![emote(Provider::SevenTv, "hat", "Hat")],
            scope.clone(),
        )
        .await;
        ready_scoped(&mut fixture).await;
        let (mut request, headers) = fixture.request().await;
        let body: Value = serde_json::from_str(headers.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert!(body["query"]
            .as_str()
            .unwrap()
            .contains("emotes { id name flags data"));
        let overlay = |set: &str, flags| {
            json!({"op": 0, "d": {"type": "emote_set.update", "body": {
                "id": set, "updated": [{"key": "emotes", "old_value": {"id": "hat", "name": "Hat"},
                    "value": {"id": "hat", "name": "Hat", "flags": flags}}]
            }}})
        };
        send(&mut fixture.socket, overlay("set-id", 1)).await;
        barrier(&mut fixture.socket, b"overlay-before-snapshot").await;
        let is_overlay = |fixture: &Harness| {
            fixture
                .index
                .read()
                .unwrap()
                .resolve(&fixture.channel, "42", "Hat")
                .unwrap()
                .zero_width
        };
        assert!(is_overlay(&fixture));
        respond(
            &mut request,
            scoped_snapshot(
                &scope,
                "set-id",
                vec![
                    active_emote("hat", "Hat"),
                    active_emote("rest", "RestMarker"),
                ],
            ),
        )
        .await;
        fixture.wait_for("RestMarker").await;
        assert!(
            is_overlay(&fixture),
            "snapshot erased a newer overlay change"
        );
        {
            let registry = fixture.index.read().unwrap();
            let catalog = registry.catalog();
            let entry = catalog.iter().find(|entry| entry.code == "Hat").unwrap();
            assert!(entry.zero_width);
            match &scope {
                LiveScope::Personal(_) => {
                    assert!(!entry.is_available_for(&fixture.channel, "other"))
                }
                LiveScope::Channel(_) => {
                    assert!(!entry.is_available_for(&ChannelId::new("other"), "42"))
                }
                LiveScope::Global => {
                    assert!(entry.is_available_for(&ChannelId::new("other"), "other"))
                }
            }
        }
        send(&mut fixture.socket, overlay("foreign-set", 0)).await;
        barrier(&mut fixture.socket, b"foreign-overlay").await;
        assert!(is_overlay(&fixture));
        send(&mut fixture.socket, overlay("set-id", 0)).await;
        barrier(&mut fixture.socket, b"disable-overlay").await;
        assert!(!is_overlay(&fixture));
        fixture.stop().await;
    }
}

#[tokio::test]
async fn seven_tv_global_and_personal_scopes_reconcile_live_changes_and_offline_removals() {
    for scope in [LiveScope::Global, LiveScope::Personal("42".into())] {
        let mut fixture = Harness::new_scoped(
            Provider::SevenTv,
            vec![emote(Provider::SevenTv, "gone", "Gone")],
            scope.clone(),
        )
        .await;
        ready_scoped(&mut fixture).await;
        let (mut request, headers) = fixture.request().await;
        let body: Value = serde_json::from_str(headers.split_once("\r\n\r\n").unwrap().1).unwrap();
        let query = body["query"].as_str().unwrap();
        assert!(query.contains("emotes { id name flags data"));
        assert_eq!(
            query.contains("userByConnection"),
            matches!(scope, LiveScope::Personal(_))
        );
        if matches!(scope, LiveScope::Personal(_)) {
            assert!(query.contains("emote_sets"));
        }
        send(&mut fixture.socket, json!({"op": 0, "d": {"type": "emote_set.update", "body": {
            "id": "set-id", "pulled": [{"key": "emotes", "old_value": {"id": "gone", "name": "Gone"}}],
            "pushed": [{"key": "emotes", "value": active_emote("live", "Live")}]
        }}})).await;
        barrier(&mut fixture.socket, b"scoped-live-change").await;
        assert!(!fixture.has("Gone"));
        assert!(fixture.has("Live"));
        respond(
            &mut request,
            scoped_snapshot(
                &scope,
                "set-id",
                vec![
                    active_emote("gone", "Gone"),
                    active_emote("rest", "RestMarker"),
                ],
            ),
        )
        .await;
        fixture.wait_for("RestMarker").await;
        assert!(!fixture.has("Gone"));
        assert!(fixture.has("Live"));
        if matches!(scope, LiveScope::Personal(_)) {
            assert!(fixture
                .index
                .read()
                .unwrap()
                .resolve(&fixture.channel, "someone-else", "Live")
                .is_none());
            assert!(fixture
                .index
                .read()
                .unwrap()
                .resolve(&ChannelId::kick("room"), "42", "Live")
                .is_none());
        }

        send(&mut fixture.socket, json!({"op": 4, "d": {}})).await;
        fixture.reconnect().await;
        ready_scoped(&mut fixture).await;
        let (mut request, _) = fixture.request().await;
        respond(
            &mut request,
            scoped_snapshot(
                &scope,
                "set-id",
                vec![active_emote("fresh", "FreshSnapshot")],
            ),
        )
        .await;
        fixture.wait_for("FreshSnapshot").await;
        assert!(
            !fixture.has("Live"),
            "reconnect retained an emote removed while offline"
        );
        assert!(!fixture.has("RestMarker"));
        fixture.stop().await;
    }
}

#[tokio::test]
async fn seven_tv_personal_assignment_changes_refresh_membership_and_replace_the_socket_binding() {
    personal_assignment_change(false).await;
}

#[tokio::test]
async fn seven_tv_personal_assignment_change_during_a_snapshot_is_not_delayed_until_periodic_refresh(
) {
    personal_assignment_change(true).await;
}

async fn personal_assignment_change(during_snapshot: bool) {
    let scope = LiveScope::Personal("42".into());
    let mut fixture = Harness::new_scoped(
        Provider::SevenTv,
        vec![emote(Provider::SevenTv, "old", "OldPersonal")],
        scope.clone(),
    )
    .await;
    ready_scoped(&mut fixture).await;
    let (mut request, _) = fixture.request().await;
    let changed = json!({"op": 0, "d": {"type": "user.update", "body": {
        "id": "user-id", "pushed": [{"key": "emote_sets", "value": {"id": "new-personal", "flags": 4}}]
    }}});
    if during_snapshot {
        send(&mut fixture.socket, changed.clone()).await;
        barrier(&mut fixture.socket, b"personal-change-during-snapshot").await;
    }
    respond(
        &mut request,
        scoped_snapshot(&scope, "set-id", vec![active_emote("rest", "Ready")]),
    )
    .await;
    fixture.wait_for("Ready").await;
    if !during_snapshot {
        send(&mut fixture.socket, changed).await;
    }
    let (mut request, _) = fixture.request().await;
    respond(
        &mut request,
        scoped_snapshot(&scope, "new-personal", Vec::new()),
    )
    .await;
    within(async {
        loop {
            if fixture.index.read().unwrap().live_targets()[0]
                .source
                .set_id
                .as_deref()
                == Some("new-personal")
            {
                break;
            }
            fixture.dirty.recv().await.unwrap();
        }
    })
    .await;
    assert!(!fixture.has("Ready"));
    let targets = fixture.index.read().unwrap().live_targets();
    fixture.desired.send(targets).unwrap();
    fixture.reconnect().await;
    ready_scoped(&mut fixture).await;
    let (mut request, headers) = fixture.request().await;
    let body: Value = serde_json::from_str(headers.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["variables"]["set"], "new-personal");
    respond(
        &mut request,
        scoped_snapshot(
            &scope,
            "new-personal",
            vec![active_emote("new", "NewPersonal")],
        ),
    )
    .await;
    fixture.wait_for("NewPersonal").await;
    send(
        &mut fixture.socket,
        json!({"op": 0, "d": {"type": "emote_set.update", "body": {
            "id": "set-id", "pushed": [{"key": "emotes", "value": active_emote("stale", "Stale")}]
        }}}),
    )
    .await;
    barrier(&mut fixture.socket, b"old-personal-set").await;
    assert!(!fixture.has("Stale"));
    fixture.stop().await;
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
async fn seven_tv_room_cosmetics_forward_in_order_and_start_a_new_epoch_after_reconnect() {
    let mut fixture = Harness::new(Provider::SevenTv, Vec::new()).await;
    ready_scoped(&mut fixture).await;
    let connected = within(fixture.cosmetics.recv()).await.unwrap();
    let SevenTvCosmeticUpdate::Live {
        epoch,
        update: SevenTvLiveCosmeticUpdate::Connected,
    } = connected
    else {
        panic!("cosmetic epoch");
    };
    let (mut request, _) = fixture.request().await;
    respond(
        &mut request,
        graphql_snapshot("set-id", vec![active_emote("ready", "Ready")]),
    )
    .await;
    fixture.wait_for("Ready").await;
    let entitlement = |event: &str| {
        json!({"op": 0, "d": {"type": event, "body": {"object": {
            "kind": "PAINT", "ref_id": "paint", "user": {"connections": [{"platform": "TWITCH", "id": "42"}]}
        }}}})
    };
    send(&mut fixture.socket, entitlement("entitlement.create")).await;
    send(&mut fixture.socket, json!({"op": 0, "d": {"type": "cosmetic.create", "body": {"object": {
        "kind": "PAINT", "data": {"id": "paint", "function": "URL", "image_url": "https://example.test/paint.webp"}
    }}}})).await;
    send(&mut fixture.socket, entitlement("entitlement.delete")).await;
    send(
        &mut fixture.socket,
        json!({"op": 0, "d": {"type": "user.update", "body": {
            "id": "user-id", "updated": [{"key": "style", "value": {"paint_id": null}}]
        }}}),
    )
    .await;
    barrier(&mut fixture.socket, b"cosmetics-reduced").await;
    let mut updates = Vec::new();
    for _ in 0..4 {
        let Some(SevenTvCosmeticUpdate::Live {
            epoch: update_epoch,
            update,
        }) = within(fixture.cosmetics.recv()).await
        else {
            panic!("live cosmetic update");
        };
        assert_eq!(update_epoch, epoch);
        updates.push(update);
    }
    assert!(
        matches!(&updates[0], SevenTvLiveCosmeticUpdate::Entitlement { granted: true, cosmetic_id, .. } if cosmetic_id == "paint")
    );
    assert!(
        matches!(&updates[1], SevenTvLiveCosmeticUpdate::Paint { id, paint } if id == "paint" && paint.function == "url")
    );
    assert!(matches!(
        &updates[2],
        SevenTvLiveCosmeticUpdate::Entitlement { granted: false, .. }
    ));
    assert!(
        matches!(&updates[3], SevenTvLiveCosmeticUpdate::RefreshUsers { twitch_user_ids } if twitch_user_ids == &["42"])
    );
    send(&mut fixture.socket, json!({"op": 4, "d": {}})).await;
    fixture.reconnect().await;
    ready_scoped(&mut fixture).await;
    assert!(matches!(within(fixture.cosmetics.recv()).await,
        Some(SevenTvCosmeticUpdate::Live { epoch: next_epoch, update: SevenTvLiveCosmeticUpdate::Connected }) if next_epoch > epoch));
    fixture.stop().await;
}

#[test]
fn seven_tv_room_cosmetics_count_toward_the_subscription_limit() {
    let target = LiveTarget {
        scope: LiveScope::Channel(ChannelId::new("room")),
        generation: 1,
        binding_generation: 1,
        source: LiveSource {
            provider: Provider::SevenTv,
            room_id: "42".to_owned(),
            platform: "TWITCH".to_owned(),
            user_id: Some("user".to_owned()),
            set_id: Some("set".to_owned()),
            connection_index: Some(0),
        },
    };
    assert!(select_targets(Provider::SevenTv, std::slice::from_ref(&target), 5).is_empty());
    assert_eq!(
        select_targets(Provider::SevenTv, std::slice::from_ref(&target), 6),
        [target]
    );
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
    for _ in 0..6 {
        subscriptions.push(next_text(&mut fixture.socket).await);
    }
    let topics: HashSet<_> = subscriptions
        .iter()
        .map(|command| {
            assert_eq!(command["op"], 35);
            (
                command["d"]["type"].as_str().unwrap(),
                command["d"]["condition"]
                    .get("object_id")
                    .or_else(|| command["d"]["condition"].get("id"))
                    .unwrap()
                    .as_str()
                    .unwrap(),
            )
        })
        .collect();
    assert_eq!(
        topics,
        HashSet::from([
            ("user.update", "user-id"),
            ("emote_set.update", "set-id"),
            ("emote_set.delete", "set-id"),
            ("cosmetic.create", "42"),
            ("entitlement.create", "42"),
            ("entitlement.delete", "42"),
        ])
    );
    for subscription in &subscriptions[..5] {
        acknowledge(&mut fixture.socket, subscription).await;
    }
    barrier(&mut fixture.socket, b"partial-acks").await;
    fixture.assert_no_request().await;
    acknowledge(&mut fixture.socket, &subscriptions[5]).await;
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
    for _ in 0..6 {
        new_subscriptions.push(next_text(&mut fixture.socket).await);
    }
    let topics: HashSet<_> = new_subscriptions
        .iter()
        .map(|command| {
            assert_eq!(command["op"], 35);
            (
                command["d"]["type"].as_str().unwrap(),
                command["d"]["condition"]
                    .get("object_id")
                    .or_else(|| command["d"]["condition"].get("id"))
                    .unwrap()
                    .as_str()
                    .unwrap(),
            )
        })
        .collect();
    assert_eq!(
        topics,
        HashSet::from([
            ("user.update", "user-id"),
            ("emote_set.update", "new-set"),
            ("emote_set.delete", "new-set"),
            ("cosmetic.create", "42"),
            ("entitlement.create", "42"),
            ("entitlement.delete", "42"),
        ])
    );
    for subscription in &new_subscriptions[..5] {
        acknowledge(&mut fixture.socket, subscription).await;
    }
    barrier(&mut fixture.socket, b"new-binding-partial-acks").await;
    fixture.assert_no_request().await;
    acknowledge(&mut fixture.socket, &new_subscriptions[5]).await;
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

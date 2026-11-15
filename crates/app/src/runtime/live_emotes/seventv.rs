use std::time::Duration;

use crust_emotes::providers::{
    parse_7tv_emote, parse_7tv_emote_set, personal_emote_set_id, read_bounded_json,
};
use serde_json::{json, Value};

use crate::seventv::{parse_7tv_live_cosmetic, SevenTvCosmeticKind, SevenTvLiveCosmeticUpdate};

use super::{EmoteDelta, LiveScope, LiveSource, LiveTarget, Snapshot};

pub(super) const URL: &str = "wss://events.7tv.io/v3";
const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct Topic {
    pub(super) event: String,
    pub(super) condition: Condition,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum Condition {
    Object(String),
    Channel(String),
}

impl Topic {
    pub(super) fn command(&self, subscribe: bool) -> String {
        let condition = match &self.condition {
            Condition::Object(id) => json!({"object_id": id}),
            Condition::Channel(id) => json!({"ctx": "channel", "platform": "TWITCH", "id": id}),
        };
        json!({"op": if subscribe { 35 } else { 36 }, "d": {
            "type": self.event, "condition": condition
        }})
        .to_string()
    }
}

pub(super) fn topics(target: &LiveTarget) -> Vec<Topic> {
    let source = &target.source;
    let mut topics = Vec::with_capacity(6);
    if let Some(user) = &source.user_id {
        topics.push(Topic {
            event: "user.update".into(),
            condition: Condition::Object(user.clone()),
        });
    }
    if let Some(set) = &source.set_id {
        topics.push(Topic {
            event: "emote_set.update".into(),
            condition: Condition::Object(set.clone()),
        });
        topics.push(Topic {
            event: "emote_set.delete".into(),
            condition: Condition::Object(set.clone()),
        });
    }
    if matches!(target.scope, LiveScope::Channel(_)) && source.platform == "TWITCH" {
        for event in [
            "cosmetic.create",
            "entitlement.create",
            "entitlement.delete",
        ] {
            topics.push(Topic {
                event: event.into(),
                condition: Condition::Channel(source.room_id.clone()),
            });
        }
    }
    topics
}

#[derive(Debug)]
pub(super) enum Frame {
    Hello {
        heartbeat: Duration,
        subscription_limit: usize,
    },
    Heartbeat,
    Reconnect,
    Subscribed(Topic),
    Updates(Vec<Update>),
}

#[derive(Debug)]
pub(super) enum Update {
    Emotes {
        set_id: String,
        deltas: Vec<EmoteDelta>,
    },
    ActiveSet {
        user_id: String,
        connection_index: usize,
        set_id: Option<String>,
    },
    DeletedSet(String),
    PersonalSetsChanged(String),
    UserStyleChanged(String),
    Cosmetic(SevenTvLiveCosmeticUpdate),
}

fn id(value: &Value) -> Option<String> {
    let value = value.as_str()?;
    (!value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    .then(|| value.to_owned())
}

fn code(value: &Value) -> Option<String> {
    let value = value.as_str()?;
    (!value.is_empty()
        && value.len() <= 256
        && !value.chars().any(|c| c.is_control() || c.is_whitespace()))
    .then(|| value.to_owned())
}

pub(super) fn decode(text: &str) -> Option<Frame> {
    if text.len() > MAX_FRAME_BYTES {
        return None;
    }
    let frame: Value = serde_json::from_str(text).ok()?;
    let body = frame.get("d")?;
    match frame.get("op")?.as_u64()? {
        1 => {
            let heartbeat = body.get("heartbeat_interval")?.as_u64()?;
            if heartbeat == 0 {
                return None;
            }
            let limit = body
                .get("subscription_limit")
                .and_then(Value::as_i64)
                .unwrap_or(-1);
            Some(Frame::Hello {
                heartbeat: Duration::from_millis(heartbeat.clamp(1_000, 120_000)),
                subscription_limit: if limit < 0 {
                    256
                } else {
                    (limit as usize).min(256)
                },
            })
        }
        2 => Some(Frame::Heartbeat),
        4 | 6 | 7 => Some(Frame::Reconnect),
        5 if body
            .get("command")?
            .as_str()?
            .eq_ignore_ascii_case("SUBSCRIBE") =>
        {
            let data = body.get("data")?;
            let event = data.get("type")?.as_str()?;
            let condition = match event {
                "user.update" | "emote_set.update" | "emote_set.delete" => {
                    Condition::Object(id(data.get("condition")?.get("object_id")?)?)
                }
                "cosmetic.create" | "entitlement.create" | "entitlement.delete" => {
                    let condition = data.get("condition")?;
                    if condition.get("ctx")?.as_str()? != "channel"
                        || condition.get("platform")?.as_str()? != "TWITCH"
                    {
                        return None;
                    }
                    Condition::Channel(id(condition.get("id")?)?)
                }
                _ => return None,
            };
            Some(Frame::Subscribed(Topic {
                event: event.into(),
                condition,
            }))
        }
        0 => decode_dispatch(body),
        _ => None,
    }
}

fn decode_dispatch(dispatch: &Value) -> Option<Frame> {
    let event = dispatch.get("type")?.as_str()?;
    let body = dispatch.get("body")?;
    if event == "cosmetic.create" {
        let object = body.get("object")?;
        let update =
            parse_7tv_live_cosmetic(object.get("kind")?.as_str()?, object.get("data")?.clone())?;
        return Some(Frame::Updates(vec![Update::Cosmetic(update)]));
    }
    if matches!(event, "entitlement.create" | "entitlement.delete") {
        let object = body.get("object")?;
        let kind = match object.get("kind")?.as_str()? {
            "PAINT" => SevenTvCosmeticKind::Paint,
            "BADGE" => SevenTvCosmeticKind::Badge,
            _ => return None,
        };
        let cosmetic_id = id(object.get("ref_id")?)?;
        let connections = object.get("user")?.get("connections")?.as_array()?;
        if connections.len() > 128 {
            return None;
        }
        let mut twitch_user_ids: Vec<_> = connections
            .iter()
            .filter(|connection| {
                connection.get("platform").and_then(Value::as_str) == Some("TWITCH")
            })
            .filter_map(|connection| connection.get("id").and_then(id))
            .collect();
        twitch_user_ids.sort();
        twitch_user_ids.dedup();
        if twitch_user_ids.is_empty() {
            return None;
        }
        return Some(Frame::Updates(vec![Update::Cosmetic(
            SevenTvLiveCosmeticUpdate::Entitlement {
                kind,
                cosmetic_id,
                twitch_user_ids,
                granted: event == "entitlement.create",
            },
        )]));
    }
    let object_id = id(body.get("id")?)?;
    match event {
        "emote_set.update" => {
            let mut deltas = Vec::new();
            for field in ["pushed", "updated", "pulled"] {
                for change in body
                    .get(field)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .take(4_096)
                {
                    if change.get("key").and_then(Value::as_str) != Some("emotes") {
                        continue;
                    }
                    let previous = change.get("old_value").unwrap_or(&Value::Null);
                    let value = change.get("value").unwrap_or(&Value::Null);
                    if field == "pulled" {
                        if let Some(emote_id) = previous.get("id").and_then(id) {
                            deltas.push(EmoteDelta::Remove {
                                id: emote_id,
                                code: previous.get("name").and_then(code),
                            });
                        }
                        continue;
                    }
                    let old_code = if field == "updated" {
                        previous.get("name").and_then(code)
                    } else {
                        None
                    };
                    if let Some(emote) = parse_7tv_emote(value.clone()) {
                        deltas.push(EmoteDelta::Upsert {
                            emote,
                            old_code,
                            preserve_overlay: field == "updated" && value.get("flags").is_none(),
                        });
                    } else if field == "updated" {
                        let Some(emote_id) =
                            value.get("id").or_else(|| previous.get("id")).and_then(id)
                        else {
                            continue;
                        };
                        let name = value.get("name").and_then(code);
                        if let Some(name) = &name {
                            deltas.push(EmoteDelta::Rename {
                                id: emote_id.clone(),
                                old_code: old_code.clone(),
                                code: name.clone(),
                            });
                        }
                        if let Some(flags) = value.get("flags").and_then(Value::as_u64) {
                            deltas.push(EmoteDelta::Overlay {
                                id: emote_id,
                                code: name.or(old_code),
                                zero_width: flags & 1 != 0,
                            });
                        }
                    }
                }
            }
            (!deltas.is_empty()).then_some(Frame::Updates(vec![Update::Emotes {
                set_id: object_id,
                deltas,
            }]))
        }
        "emote_set.delete" => Some(Frame::Updates(vec![Update::DeletedSet(object_id)])),
        "user.update" => {
            let mut changes = Vec::new();
            for connection in body
                .get("updated")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .take(128)
            {
                if connection.get("key").and_then(Value::as_str) != Some("connections") {
                    continue;
                }
                let Some(index) = connection
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|n| usize::try_from(n).ok())
                else {
                    continue;
                };
                let Some(fields) = connection.get("value").and_then(Value::as_array) else {
                    continue;
                };
                let selected = fields
                    .iter()
                    .find(|field| field.get("key").and_then(Value::as_str) == Some("emote_set_id"))
                    .and_then(|field| field.get("value"))
                    .or_else(|| {
                        fields
                            .iter()
                            .find(|field| {
                                field.get("key").and_then(Value::as_str) == Some("emote_set")
                            })
                            .and_then(|field| field.get("value"))
                            .and_then(|value| {
                                if value.is_null() {
                                    Some(value)
                                } else {
                                    value.get("id")
                                }
                            })
                    });
                let Some(selected) = selected else {
                    continue;
                };
                let set_id = if selected.is_null() {
                    None
                } else {
                    let Some(selected) = id(selected) else {
                        continue;
                    };
                    Some(selected)
                };
                changes.push(Update::ActiveSet {
                    user_id: object_id.clone(),
                    connection_index: index,
                    set_id,
                });
            }
            if ["updated", "pushed", "pulled"].into_iter().any(|field| {
                body.get(field)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .take(128)
                    .any(|change| change.get("key").and_then(Value::as_str) == Some("emote_sets"))
            }) {
                changes.push(Update::PersonalSetsChanged(object_id.clone()));
            }
            if ["updated", "pushed", "pulled"].into_iter().any(|field| {
                body.get(field)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .take(128)
                    .any(|change| {
                        matches!(
                            change.get("key").and_then(Value::as_str),
                            Some("style" | "avatar_url")
                        )
                    })
            }) {
                changes.push(Update::UserStyleChanged(object_id));
            }
            (!changes.is_empty()).then_some(Frame::Updates(changes))
        }
        _ => None,
    }
}

/// POST avoids the REST edge cache. Reconcile buffered socket changes even when
/// the response is current, because a set can change while the request is in flight.
pub(super) async fn snapshot_at(
    client: &reqwest::Client,
    endpoint: &str,
    scope: &LiveScope,
    source: &LiveSource,
) -> Option<Snapshot> {
    if !matches!(scope, LiveScope::Global) && !matches!(source.platform.as_str(), "TWITCH" | "KICK")
    {
        return None;
    }
    if matches!(scope, LiveScope::Global) && source.set_id.is_none() {
        return Some(Snapshot::Emotes(Vec::new()));
    }
    let set_var = if source.set_id.is_some() {
        ", $set: ObjectID!"
    } else {
        ""
    };
    let set_query = if source.set_id.is_some() {
        "sets: emoteSetsByID(list: [$set]) { id emotes { id name flags data { host { url files { name width } } } } }"
    } else {
        ""
    };
    let query = match scope {
        LiveScope::Channel(_) => format!("query($room: String!{set_var}) {{ userByConnection(platform: {}, id: $room) {{ id connections {{ id platform emote_set_id }} }} {set_query} }}", source.platform),
        LiveScope::Personal(_) => format!("query($room: String!{set_var}) {{ userByConnection(platform: TWITCH, id: $room) {{ id emote_sets {{ id flags }} }} {set_query} }}"),
        LiveScope::Global => format!("query($set: ObjectID!) {{ {set_query} }}"),
    };
    let response = client
        .post(endpoint)
        .json(&json!({"query": query,
        "variables": {"room": source.room_id, "set": source.set_id}}))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let value = read_bounded_json::<Value>(response, 8 * 1024 * 1024).await?;
    if value
        .get("errors")
        .and_then(Value::as_array)
        .is_some_and(|errors| !errors.is_empty())
    {
        return None;
    }
    let data = value.get("data")?;
    let mut selected = source.clone();
    if !matches!(scope, LiveScope::Global) {
        let user = data.get("userByConnection")?;
        if user.is_null() {
            selected.user_id = None;
            selected.set_id = None;
            selected.connection_index = None;
        } else {
            selected.user_id = Some(id(user.get("id")?)?);
            if matches!(scope, LiveScope::Personal(_)) {
                let sets = user.get("emote_sets")?.as_array()?;
                if sets.len() > 64
                    || sets.iter().any(|set| {
                        set.get("id").and_then(id).is_none()
                            || set.get("flags").and_then(Value::as_u64).is_none()
                    })
                {
                    return None;
                }
                selected.set_id = personal_emote_set_id(user).map(str::to_owned);
                selected.connection_index = None;
            } else {
                let (index, connection) = user
                    .get("connections")?
                    .as_array()?
                    .iter()
                    .enumerate()
                    .find(|(_, connection)| {
                    connection.get("id").and_then(Value::as_str) == Some(source.room_id.as_str())
                        && connection.get("platform").and_then(Value::as_str)
                            == Some(source.platform.as_str())
                })?;
                selected.connection_index = Some(index);
                let value = connection.get("emote_set_id")?;
                selected.set_id = if value.is_null() {
                    None
                } else {
                    Some(id(value)?)
                };
            }
        }
    }
    if &selected != source {
        return Some(Snapshot::Source(selected));
    }
    let Some(set_id) = &source.set_id else {
        return Some(Snapshot::Emotes(Vec::new()));
    };
    let set = data
        .get("sets")?
        .as_array()?
        .iter()
        .find(|set| set.get("id").and_then(Value::as_str) == Some(set_id.as_str()))?;
    Some(Snapshot::Emotes(parse_7tv_emote_set(set.clone())?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_hello_and_ack_envelopes_control_subscriptions() {
        let Some(Frame::Hello {
            heartbeat,
            subscription_limit,
        }) = decode(r#"{"op":1,"d":{"heartbeat_interval":25000,"subscription_limit":100}}"#)
        else {
            panic!("hello");
        };
        assert_eq!(heartbeat, Duration::from_secs(25));
        assert_eq!(subscription_limit, 100);
        let topic = Topic {
            event: "emote_set.update".into(),
            condition: Condition::Object("set".into()),
        };
        let command: Value = serde_json::from_str(&topic.command(true)).unwrap();
        assert_eq!(command["op"], 35);
        assert_eq!(command["d"]["condition"]["object_id"], "set");
        assert!(
            matches!(decode(r#"{"op":5,"d":{"command":"SUBSCRIBE","data":{"type":"emote_set.update","condition":{"object_id":"set"}}}}"#), Some(Frame::Subscribed(received)) if received == topic)
        );
        assert!(decode(r#"{"op":1,"d":{"heartbeat_interval":0}}"#).is_none());
    }

    #[test]
    fn room_cosmetics_use_channel_conditions_and_current_ack_envelopes() {
        let topic = Topic {
            event: "cosmetic.create".into(),
            condition: Condition::Channel("42".into()),
        };
        let command: Value = serde_json::from_str(&topic.command(true)).unwrap();
        assert_eq!(
            command["d"]["condition"],
            json!({"ctx": "channel", "platform": "TWITCH", "id": "42"})
        );
        let ack = json!({"op": 5, "d": {"command": "SUBSCRIBE", "data": command["d"]}});
        assert!(
            matches!(decode(&ack.to_string()), Some(Frame::Subscribed(received)) if received == topic)
        );
        let mut wrong_platform = ack.clone();
        wrong_platform["d"]["data"]["condition"]["platform"] = json!("KICK");
        assert!(decode(&wrong_platform.to_string()).is_none());
        let unsubscribe: Value = serde_json::from_str(&topic.command(false)).unwrap();
        assert_eq!(unsubscribe["op"], 36);
    }

    #[test]
    fn cosmetic_and_entitlement_frames_keep_ids_and_rgba_without_outer_object_id() {
        let cosmetic = json!({"op": 0, "d": {"type": "cosmetic.create", "body": {"object": {
            "kind": "PAINT", "data": {"id": "paint", "function": "LINEAR_GRADIENT", "stops": [
                {"at": 0, "color": 0}, {"at": 1, "color": 0xFFFFFFFFu32}
            ]}
        }}}});
        let Some(Frame::Updates(updates)) = decode(&cosmetic.to_string()) else {
            panic!("paint dispatch");
        };
        assert!(
            matches!(&updates[0], Update::Cosmetic(SevenTvLiveCosmeticUpdate::Paint { id, paint })
            if id == "paint" && paint.stops[0].color == "#00000000" && paint.stops[1].color == "#FFFFFF")
        );
        for event in ["entitlement.create", "entitlement.delete"] {
            let entitlement = json!({"op": 0, "d": {"type": event, "body": {"object": {
                "kind": "PAINT", "ref_id": "paint", "user": {"connections": [
                    {"platform": "TWITCH", "id": "42"}, {"platform": "KICK", "id": "99"},
                    {"platform": "TWITCH", "id": "42"}
                ]}
            }}}});
            let Some(Frame::Updates(updates)) = decode(&entitlement.to_string()) else {
                panic!("entitlement dispatch");
            };
            assert!(
                matches!(&updates[0], Update::Cosmetic(SevenTvLiveCosmeticUpdate::Entitlement {
                kind: SevenTvCosmeticKind::Paint, cosmetic_id, twitch_user_ids, granted,
            }) if cosmetic_id == "paint" && twitch_user_ids == &["42"] && *granted == (event == "entitlement.create"))
            );
        }
    }

    #[test]
    fn malformed_entitlements_do_not_request_a_refresh_or_clear_a_selection() {
        for object in [
            json!({"kind": "PAINT", "ref_id": "paint", "user": {"connections": [{"platform": "KICK", "id": "42"}]}}),
            json!({"kind": "PAINT", "ref_id": "", "user": {"connections": [{"platform": "TWITCH", "id": "42"}]}}),
            json!({"kind": "BADGE", "ref_id": "badge", "user": {"connections": [{"platform": "TWITCH", "id": "invalid user"}]}}),
        ] {
            assert!(decode(
                &json!({"op": 0, "d": {"type": "entitlement.delete", "body": {"object": object}}})
                    .to_string()
            )
            .is_none());
        }
    }

    #[test]
    fn updates_preserve_alias_identity_and_accept_renames_without_owner_metadata() {
        let value = json!({"op":0,"d":{"type":"emote_set.update","body":{"id":"set",
            "updated":[{"key":"emotes","old_value":{"id":"id","name":"Old"},"value":{"id":"id","name":"New"}}],
            "pulled":[{"key":"emotes","old_value":{"id":"other","name":"Gone"}}]}}});
        let Some(Frame::Updates(updates)) = decode(&value.to_string()) else {
            panic!("dispatch");
        };
        let Update::Emotes { set_id, deltas } = &updates[0] else {
            panic!("emotes");
        };
        assert_eq!(set_id, "set");
        assert!(
            matches!(&deltas[0], EmoteDelta::Rename { old_code: Some(old), code, .. } if old == "Old" && code == "New")
        );
        assert!(
            matches!(&deltas[1], EmoteDelta::Remove { id, code: Some(name) } if id == "other" && name == "Gone")
        );
    }

    #[test]
    fn active_set_changes_include_connection_index_and_explicit_removal() {
        let value = json!({"op":0,"d":{"type":"user.update","body":{"id":"user","updated":[
            {"key":"connections","index":1,"nested":true,"value":[{"key":"emote_set","value":{"id":"new"}}]},
            {"key":"connections","index":2,"nested":true,"value":[{"key":"emote_set_id","value":null}]}]}}});
        let Some(Frame::Updates(updates)) = decode(&value.to_string()) else {
            panic!("dispatch");
        };
        assert!(
            matches!(&updates[0], Update::ActiveSet { user_id, connection_index: 1, set_id: Some(set) } if user_id == "user" && set == "new")
        );
        assert!(matches!(
            &updates[1],
            Update::ActiveSet {
                connection_index: 2,
                set_id: None,
                ..
            }
        ));
    }

    #[test]
    fn overlay_updates_accept_partial_flags_and_alias_changes() {
        for (value, expected_code, enabled) in [
            (json!({"flags": 1}), "Old", true),
            (json!({"id": "id", "name": "New", "flags": 0}), "New", false),
            (json!({"id": "id", "flags": 256}), "Old", false),
        ] {
            let frame = json!({"op": 0, "d": {"type": "emote_set.update", "body": {
                "id": "set", "updated": [{"key": "emotes",
                    "old_value": {"id": "id", "name": "Old"}, "value": value}]
            }}});
            let Some(Frame::Updates(updates)) = decode(&frame.to_string()) else {
                panic!("dispatch");
            };
            let Update::Emotes { deltas, .. } = &updates[0] else {
                panic!("emotes");
            };
            assert!(deltas.iter().any(|delta| matches!(delta,
                EmoteDelta::Overlay { id, code: Some(code), zero_width }
                if id == "id" && code == expected_code && *zero_width == enabled)));
        }
        let data_only = json!({"op": 0, "d": {"type": "emote_set.update", "body": {
            "id": "set", "updated": [{"key": "emotes", "value": {
                "id": "id", "data": {"flags": 256}
            }}]
        }}});
        assert!(decode(&data_only.to_string()).is_none());
        for flags in [None, Some(0), Some(1)] {
            let mut value = json!({"id": "id", "name": "Name", "data": {"host": {
                "url": "//cdn.7tv.app/emote/id", "files": [{"name": "2x.webp", "width": 64}]
            }}});
            if let Some(flags) = flags {
                value["flags"] = flags.into();
            }
            let frame = json!({"op": 0, "d": {"type": "emote_set.update", "body": {
                "id": "set", "updated": [{"key": "emotes", "value": value}]
            }}});
            let Some(Frame::Updates(updates)) = decode(&frame.to_string()) else {
                panic!("dispatch");
            };
            let Update::Emotes { deltas, .. } = &updates[0] else {
                panic!("emotes");
            };
            assert!(
                matches!(&deltas[0], EmoteDelta::Upsert { emote, preserve_overlay, .. }
                if *preserve_overlay == flags.is_none() && emote.zero_width == (flags == Some(1)))
            );
        }
    }
}

use crust_emotes::EmoteInfo;
use serde::Deserialize;
use serde_json::Value;

use super::EmoteDelta;

pub(super) const URL: &str = "wss://sockets.betterttv.net/ws";
const MAX_FRAME_BYTES: usize = 64 * 1024;
const MAX_ID_BYTES: usize = 128;
const MAX_CODE_BYTES: usize = 128;

#[derive(Deserialize)]
struct Envelope {
    name: String,
    data: Value,
}

pub(super) fn subscribe(room_id: &str) -> String {
    command("join_channel", room_id)
}

pub(super) fn unsubscribe(room_id: &str) -> String {
    command("part_channel", room_id)
}

fn command(name: &str, room_id: &str) -> String {
    serde_json::json!({"name": name, "data": {"name": format!("twitch:{room_id}")}}).to_string()
}

/// The destination stays attached to the mutation until the worker checks membership.
pub(super) fn decode(text: &str) -> Option<(String, Vec<EmoteDelta>)> {
    if text.len() > MAX_FRAME_BYTES {
        return None;
    }
    let envelope: Envelope = serde_json::from_str(text).ok()?;
    let room = twitch_room(envelope.data.get("channel")?.as_str()?)?;
    let delta = match envelope.name.as_str() {
        "emote_create" => EmoteDelta::Upsert {
            emote: parse_emote(envelope.data.get("emote")?)?,
            old_code: None,
            preserve_overlay: false,
        },
        "emote_update" => {
            let emote = envelope.data.get("emote")?;
            let id = emote.get("id")?.as_str().filter(|id| valid_id(id))?;
            // Updates are partial. A rename must retain the existing image URLs;
            // animated/user-only changes do not affect the stored EmoteInfo.
            let code = emote
                .get("code")?
                .as_str()
                .filter(|code| valid_code(code))?;
            EmoteDelta::Rename {
                id: id.to_owned(),
                old_code: None,
                code: code.to_owned(),
            }
        }
        "emote_delete" => {
            let id = envelope
                .data
                .get("emoteId")?
                .as_str()
                .filter(|id| valid_id(id))?;
            EmoteDelta::Remove {
                id: id.to_owned(),
                code: None,
            }
        }
        _ => return None,
    };
    Some((room, vec![delta]))
}

pub(super) fn parse_emote(emote: &Value) -> Option<EmoteInfo> {
    let id = emote.get("id")?.as_str().filter(|id| valid_id(id))?;
    let code = emote
        .get("code")?
        .as_str()
        .filter(|code| valid_code(code))?;
    Some(EmoteInfo {
        id: id.to_owned(),
        code: code.to_owned(),
        provider: "bttv".to_owned(),
        zero_width: false,
        url_1x: format!("https://cdn.betterttv.net/emote/{id}/1x"),
        url_2x: Some(format!("https://cdn.betterttv.net/emote/{id}/2x")),
        url_4x: Some(format!("https://cdn.betterttv.net/emote/{id}/3x")),
    })
}

fn twitch_room(channel: &str) -> Option<String> {
    let id = channel.strip_prefix("twitch:")?;
    if id.is_empty() || id.len() > 20 || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let id = id.parse::<u64>().ok()?;
    (id != 0).then(|| id.to_string())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= MAX_CODE_BYTES
        && !code
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMOTE_ID: &str = "566ca06065dbbdab32ec054a";

    fn frame(name: &str, data: Value) -> String {
        serde_json::json!({"name": name, "data": data}).to_string()
    }

    #[test]
    fn subscriptions_use_the_public_twitch_channel_envelope() {
        assert_eq!(
            serde_json::from_str::<Value>(&subscribe("1234")).unwrap(),
            serde_json::json!({"name": "join_channel", "data": {"name": "twitch:1234"}})
        );
        assert_eq!(
            serde_json::from_str::<Value>(&unsubscribe("1234")).unwrap(),
            serde_json::json!({"name": "part_channel", "data": {"name": "twitch:1234"}})
        );
        assert_eq!(URL, "wss://sockets.betterttv.net/ws");
    }

    #[test]
    fn create_keeps_room_identity_and_builds_all_existing_provider_asset_sizes() {
        let encoded = frame(
            "emote_create",
            serde_json::json!({
                "channel": "twitch:1234", "emote": {"id": EMOTE_ID, "code": "HelloWave",
                    "animated": true, "user": {"id": "owner", "name": "Owner"}}
            }),
        );
        let (room, deltas) = decode(&encoded).unwrap();
        assert_eq!(room, "1234");
        let [EmoteDelta::Upsert { emote, old_code, .. }] = deltas.as_slice() else {
            panic!("expected create");
        };
        assert!(old_code.is_none());
        assert_eq!(emote.id, EMOTE_ID);
        assert_eq!(emote.code, "HelloWave");
        assert_eq!(emote.provider, "bttv");
        assert_eq!(
            emote.url_1x,
            format!("https://cdn.betterttv.net/emote/{EMOTE_ID}/1x")
        );
        assert_eq!(
            emote.url_2x,
            Some(format!("https://cdn.betterttv.net/emote/{EMOTE_ID}/2x"))
        );
        assert_eq!(
            emote.url_4x,
            Some(format!("https://cdn.betterttv.net/emote/{EMOTE_ID}/3x"))
        );
    }

    #[test]
    fn partial_renames_target_the_id_without_replacing_absent_image_metadata() {
        let encoded = frame(
            "emote_update",
            serde_json::json!({
                "channel": "twitch:1234", "emote": {"id": EMOTE_ID, "code": "NewAlias"}
            }),
        );
        let (_, deltas) = decode(&encoded).unwrap();
        let [EmoteDelta::Rename { id, old_code, code }] = deltas.as_slice() else {
            panic!("expected rename");
        };
        assert_eq!(id, EMOTE_ID);
        assert!(old_code.is_none());
        assert_eq!(code, "NewAlias");
        for emote in [
            serde_json::json!({"id": EMOTE_ID, "animated": false}),
            serde_json::json!({"id": EMOTE_ID, "user": {"name": "Owner"}}),
            serde_json::json!({"id": EMOTE_ID}),
        ] {
            assert!(decode(&frame(
                "emote_update",
                serde_json::json!({
                    "channel": "twitch:1234", "emote": emote
                })
            ))
            .is_none());
        }
    }

    #[test]
    fn delete_targets_the_id_and_foreign_twitch_rooms_remain_distinct() {
        let encoded = frame(
            "emote_delete",
            serde_json::json!({
                "channel": "twitch:5678", "emoteId": EMOTE_ID
            }),
        );
        let (room, deltas) = decode(&encoded).unwrap();
        assert_eq!(room, "5678");
        assert_ne!(room, "1234");
        let [EmoteDelta::Remove { id, code }] = deltas.as_slice() else {
            panic!("expected delete");
        };
        assert_eq!(id, EMOTE_ID);
        assert!(code.is_none());
        assert!(decode(&frame(
            "emote_delete",
            serde_json::json!({
                "channel": "kick:1234", "emoteId": EMOTE_ID
            })
        ))
        .is_none());
    }

    #[test]
    fn malformed_fields_and_bounded_ids_cannot_create_mutations() {
        for channel in [
            "",
            "twitch:",
            "twitch:abc",
            "twitch:-1",
            "twitch:+1",
            "twitch:0",
            "twitch:1234:other",
            "twitch:18446744073709551616",
        ] {
            assert!(
                decode(&frame(
                    "emote_delete",
                    serde_json::json!({
                        "channel": channel, "emoteId": EMOTE_ID
                    })
                ))
                .is_none(),
                "accepted invalid room {channel:?}"
            );
        }
        for id in [
            "",
            "../id",
            "id/path",
            "id:other",
            "id?query",
            "id fragment",
            "é",
        ] {
            assert!(
                decode(&frame(
                    "emote_delete",
                    serde_json::json!({
                        "channel": "twitch:1234", "emoteId": id
                    })
                ))
                .is_none(),
                "accepted invalid ID {id:?}"
            );
        }
        for code in ["", "two words", "line\nfeed", "tab\tcode", "null\0code"] {
            assert!(
                decode(&frame(
                    "emote_create",
                    serde_json::json!({
                        "channel": "twitch:1234", "emote": {"id": EMOTE_ID, "code": code}
                    })
                ))
                .is_none(),
                "accepted invalid code {code:?}"
            );
        }
        for (id, code) in [
            ("a".repeat(MAX_ID_BYTES + 1), "Valid".into()),
            (EMOTE_ID.into(), "a".repeat(MAX_CODE_BYTES + 1)),
        ] {
            assert!(decode(&frame(
                "emote_create",
                serde_json::json!({
                    "channel": "twitch:1234", "emote": {"id": id, "code": code}
                })
            ))
            .is_none());
        }
        for text in [
            "",
            "{",
            "null",
            "[]",
            r#"{"name":"emote_create","data":{"channel":"twitch:1234","emote":{"id":3,"code":"Wave"}}}"#,
        ] {
            assert!(decode(text).is_none());
        }
        assert!(decode(&" ".repeat(MAX_FRAME_BYTES + 1)).is_none());
        assert!(decode(&frame(
            "lookup_user",
            serde_json::json!({"channel": "twitch:1234"})
        ))
        .is_none());
    }
}

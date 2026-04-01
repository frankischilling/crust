use crust_twitch::{parse_line, parse_privmsg_irc};

#[test]
fn parse_privmsg_prefers_id_for_server_message_id() {
    let raw = "@badge-info=;badges=;color=#9146FF;display-name=UserA;id=primary-id-123;source-id=source-id-999;tmi-sent-ts=1735736404999;user-id=111 :usera!usera@usera.tmi.twitch.tv PRIVMSG #rustlang :hello world";
    let msg = parse_line(raw).expect("parse line");
    let chat = parse_privmsg_irc(&msg, None, 10).expect("parse privmsg");

    assert_eq!(chat.server_id.as_deref(), Some("primary-id-123"));
}

#[test]
fn parse_privmsg_uses_source_id_when_id_is_missing() {
    let raw = "@badge-info=;badges=;color=#00AA88;display-name=UserB;source-id=source-only-456;tmi-sent-ts=1735736405999;user-id=222 :userb!userb@userb.tmi.twitch.tv PRIVMSG #rustlang :new message";
    let msg = parse_line(raw).expect("parse line");
    let chat = parse_privmsg_irc(&msg, None, 11).expect("parse privmsg");

    assert_eq!(chat.server_id.as_deref(), Some("source-only-456"));
}

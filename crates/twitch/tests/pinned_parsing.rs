use crust_twitch::{parse_line, parse_privmsg_irc};

#[test]
fn parse_privmsg_marks_pinned_when_level_tag_exists() {
    let raw = "@badge-info=;badges=subscriber/6;color=#00FF7F;display-name=PinnedUser;id=xp1;pinned-chat-paid-level=ONE;tmi-sent-ts=1735736401999;user-id=12345 :pinneduser!pinneduser@pinneduser.tmi.twitch.tv PRIVMSG #rustlang :hello pinned";
    let msg = parse_line(raw).expect("parse line");
    let chat = parse_privmsg_irc(&msg, None, 1).expect("parse privmsg");

    assert!(
        chat.flags.is_pinned,
        "expected pinned-chat-paid-level to mark message as pinned"
    );
}

#[test]
fn parse_privmsg_marks_pinned_when_msg_id_mentions_pinned_chat() {
    let raw = "@badge-info=;badges=;color=#9146FF;display-name=PinnedUser;id=xp2;msg-id=pinned-chat-paid-level;tmi-sent-ts=1735736402999;user-id=54321 :pinneduser!pinneduser@pinneduser.tmi.twitch.tv PRIVMSG #rustlang :hello pinned by msg-id";
    let msg = parse_line(raw).expect("parse line");
    let chat = parse_privmsg_irc(&msg, None, 2).expect("parse privmsg");

    assert!(
        chat.flags.is_pinned,
        "expected pinned-chat msg-id to mark message as pinned"
    );
}

#[test]
fn parse_privmsg_marks_pinned_for_any_pinned_chat_tag_family_key() {
    let raw = "@badge-info=;badges=;color=#9146FF;display-name=PinnedUser;id=xp3;pinned-chat-paid-canonical-amount=1400;tmi-sent-ts=1735736403999;user-id=67890 :pinneduser!pinneduser@pinneduser.tmi.twitch.tv PRIVMSG #rustlang :hello pinned by canonical amount";
    let msg = parse_line(raw).expect("parse line");
    let chat = parse_privmsg_irc(&msg, None, 3).expect("parse privmsg");

    assert!(
        chat.flags.is_pinned,
        "expected pinned-chat-paid-* family tags to mark message as pinned"
    );
}

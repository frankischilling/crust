use std::time::{Duration, Instant};

use crust_twitch::irc::split_and_parse;
use crust_twitch::{parse_line, parse_privmsg_irc};

fn report(label: &str, ops: usize, elapsed: Duration) {
    let secs = elapsed.as_secs_f64();
    let ops_per_sec = if secs > 0.0 { ops as f64 / secs } else { 0.0 };
    println!(
        "[perf] {label}: {ops} ops in {:?} ({ops_per_sec:.0} ops/s)",
        elapsed
    );
}

const SAMPLE_PRIVMSG: &str = "@badge-info=subscriber/12;badges=subscriber/12;color=#1E90FF;display-name=RustFan;emotes=25:0-4;first-msg=0;id=abc123;tmi-sent-ts=1735736400123;user-id=123456 :rustfan!rustfan@rustfan.tmi.twitch.tv PRIVMSG #rustlang :Kappa this parser should be fast";

#[test]
#[ignore]
fn perf_parse_line_privmsg() {
    for _ in 0..3_000 {
        let _ = parse_line(SAMPLE_PRIVMSG).expect("warmup parse_line failed");
    }

    let iterations = 220_000usize;
    let start = Instant::now();
    let mut checksum = 0usize;
    for _ in 0..iterations {
        let msg = parse_line(SAMPLE_PRIVMSG).expect("parse_line failed");
        checksum = checksum.wrapping_add(msg.params.len() + msg.command.len());
    }
    let elapsed = start.elapsed();

    report("twitch::irc::parse_line", iterations, elapsed);
    assert!(checksum > 0);
}

#[test]
#[ignore]
fn perf_parse_privmsg_irc_history_conversion() {
    let msg = parse_line(SAMPLE_PRIVMSG).expect("fixture parse_line failed");

    for i in 0..3_000u64 {
        let _ = parse_privmsg_irc(&msg, Some("rustfan"), i).expect("warmup parse_privmsg_irc");
    }

    let iterations = 170_000usize;
    let start = Instant::now();
    let mut checksum = 0usize;
    for i in 0..iterations as u64 {
        let chat = parse_privmsg_irc(&msg, Some("rustfan"), i).expect("parse_privmsg_irc failed");
        checksum = checksum.wrapping_add(chat.raw_text.len() + chat.sender.badges.len());
    }
    let elapsed = start.elapsed();

    report("twitch::session::parse_privmsg_irc", iterations, elapsed);
    assert!(checksum > 0);
}

#[test]
#[ignore]
fn perf_split_and_parse_batch_frame() {
    let frame = concat!(
        "PING :tmi.twitch.tv\r\n",
        "@badge-info=subscriber/6;badges=subscriber/6;color=#00FF7F;display-name=SpeedUser;id=x1;tmi-sent-ts=1735736400999 :speed!speed@speed.tmi.twitch.tv PRIVMSG #rustlang :hello chat\r\n",
        "@badge-info=;badges=moderator/1;color=#FF69B4;display-name=ModUser;id=x2;tmi-sent-ts=1735736401999 :mod!mod@mod.tmi.twitch.tv PRIVMSG #rustlang :please follow the rules\r\n"
    );

    for _ in 0..2_000 {
        let _ = split_and_parse(frame);
    }

    let iterations = 120_000usize;
    let start = Instant::now();
    let mut checksum = 0usize;
    for _ in 0..iterations {
        let parsed = split_and_parse(frame);
        checksum = checksum.wrapping_add(parsed.len());
        for msg in parsed.into_iter().flatten() {
            checksum = checksum.wrapping_add(msg.params.len());
        }
    }
    let elapsed = start.elapsed();

    report("twitch::irc::split_and_parse", iterations, elapsed);
    assert!(checksum > 0);
}

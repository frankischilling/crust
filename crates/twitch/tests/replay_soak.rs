use std::collections::VecDeque;
use std::env;
use std::thread;
use std::time::{Duration, Instant};

use crust_core::format::tokenize;
use crust_core::highlight::{is_highlighted, HighlightRule};
use crust_twitch::{parse_line, parse_privmsg_irc};

const FRAME_HZ: u64 = 60;
const MAX_EVENTS_PER_FRAME: usize = 200;

const DEFAULT_SOAK_SECS: u64 = 60;
const DEFAULT_MSGS_PER_SEC: usize = 200;

const SAMPLE_PRIVMSGS: [&str; 6] = [
    "@badge-info=subscriber/12;badges=subscriber/12;color=#1E90FF;display-name=RustFan;emotes=25:0-4;first-msg=0;id=abc123;tmi-sent-ts=1735736400123;user-id=123456 :rustfan!rustfan@rustfan.tmi.twitch.tv PRIVMSG #rustlang :Kappa parser speed test",
    "@badge-info=;badges=moderator/1;color=#00FF7F;display-name=ModUser;id=abc124;tmi-sent-ts=1735736401123;user-id=234567 :moduser!moduser@moduser.tmi.twitch.tv PRIVMSG #rustlang :please read the rules",
    "@badge-info=;badges=vip/1;color=#FF69B4;display-name=VipUser;id=abc125;tmi-sent-ts=1735736402123;user-id=345678 :vipuser!vipuser@vipuser.tmi.twitch.tv PRIVMSG #rustlang :PogChamp this is cool",
    "@badge-info=;badges=;color=#FFD700;display-name=ViewerOne;id=abc126;tmi-sent-ts=1735736403123;user-id=456789 :viewerone!viewerone@viewerone.tmi.twitch.tv PRIVMSG #rustlang :check this clip https://clips.twitch.tv/FancyClip",
    "@badge-info=;badges=subscriber/3;color=#40E0D0;display-name=EmojiUser;id=abc127;tmi-sent-ts=1735736404123;user-id=567890 :emojiuser!emojiuser@emojiuser.tmi.twitch.tv PRIVMSG #rustlang :hello 😀 everyone",
    "@badge-info=;badges=;color=#ADFF2F;display-name=ReplyUser;reply-parent-msg-id=deadbeef;reply-parent-user-login=rustfan;reply-parent-display-name=RustFan;reply-parent-msg-body=hi;id=abc128;tmi-sent-ts=1735736405123;user-id=678901 :replyuser!replyuser@replyuser.tmi.twitch.tv PRIVMSG #rustlang :@RustFan agreed",
];

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
}

fn emote_lookup(word: &str) -> Option<(String, String, String, String, Option<String>)> {
    match word {
        "Kappa" => Some((
            "25".to_owned(),
            "Kappa".to_owned(),
            "https://example.local/kappa.png".to_owned(),
            "twitch".to_owned(),
            Some("https://example.local/kappa-4x.png".to_owned()),
        )),
        "PogChamp" => Some((
            "88".to_owned(),
            "PogChamp".to_owned(),
            "https://example.local/pogchamp.png".to_owned(),
            "7tv".to_owned(),
            Some("https://example.local/pogchamp-4x.png".to_owned()),
        )),
        _ => None,
    }
}

#[test]
#[ignore]
fn replay_soak_maintains_headroom() {
    let msgs_per_sec = env_usize("CRUST_SOAK_RATE", DEFAULT_MSGS_PER_SEC);
    let soak_secs = env_u64("CRUST_SOAK_SECS", DEFAULT_SOAK_SECS);

    let frame_budget = Duration::from_secs_f64(1.0 / FRAME_HZ as f64);
    let soak_duration = Duration::from_secs(soak_secs);

    let highlight_rules = vec![
        HighlightRule::new("raid"),
        HighlightRule::new("giveaway"),
        HighlightRule::new("@rustfan"),
        HighlightRule::new("PogChamp"),
    ];

    let mut queue: VecDeque<usize> = VecDeque::new();

    let mut produced = 0usize;
    let mut consumed = 0usize;
    let mut parse_errors = 0usize;

    let mut max_backlog = 0usize;
    let mut max_frame_work = Duration::ZERO;
    let mut peak_processed_frame = 0usize;
    let mut span_checksum = 0usize;

    let start = Instant::now();
    let mut next_frame = start + frame_budget;

    println!(
        "[soak] start: rate={} msg/s, duration={}s, frame_hz={}, frame_cap={}",
        msgs_per_sec, soak_secs, FRAME_HZ, MAX_EVENTS_PER_FRAME
    );

    while start.elapsed() < soak_duration {
        let target_total = (start.elapsed().as_secs_f64() * msgs_per_sec as f64) as usize;
        while produced < target_total {
            queue.push_back(produced % SAMPLE_PRIVMSGS.len());
            produced += 1;
        }

        max_backlog = max_backlog.max(queue.len());

        let frame_work_start = Instant::now();
        let mut processed_this_frame = 0usize;

        while processed_this_frame < MAX_EVENTS_PER_FRAME {
            let Some(sample_idx) = queue.pop_front() else {
                break;
            };

            let line = SAMPLE_PRIVMSGS[sample_idx];
            match parse_line(line)
                .ok()
                .and_then(|msg| parse_privmsg_irc(&msg, Some("rustfan"), consumed as u64 + 1))
            {
                Some(chat) => {
                    let spans = tokenize(
                        &chat.raw_text,
                        chat.flags.is_action,
                        &chat.twitch_emotes,
                        &emote_lookup,
                    );
                    let _ = is_highlighted(&highlight_rules, &chat.raw_text);
                    span_checksum = span_checksum.wrapping_add(spans.len());
                    consumed += 1;
                }
                None => {
                    parse_errors += 1;
                }
            }

            processed_this_frame += 1;
        }

        peak_processed_frame = peak_processed_frame.max(processed_this_frame);

        let frame_work = frame_work_start.elapsed();
        max_frame_work = max_frame_work.max(frame_work);

        let now = Instant::now();
        if now < next_frame {
            thread::sleep(next_frame - now);
        }

        let now = Instant::now();
        if now > next_frame + frame_budget {
            next_frame = now + frame_budget;
        } else {
            next_frame += frame_budget;
        }
    }

    let drain_start = Instant::now();
    while !queue.is_empty() && drain_start.elapsed() < Duration::from_secs(2) {
        let sample_idx = queue.pop_front().expect("queue checked non-empty");
        let line = SAMPLE_PRIVMSGS[sample_idx];

        match parse_line(line)
            .ok()
            .and_then(|msg| parse_privmsg_irc(&msg, Some("rustfan"), consumed as u64 + 1))
        {
            Some(chat) => {
                let spans = tokenize(
                    &chat.raw_text,
                    chat.flags.is_action,
                    &chat.twitch_emotes,
                    &emote_lookup,
                );
                let _ = is_highlighted(&highlight_rules, &chat.raw_text);
                span_checksum = span_checksum.wrapping_add(spans.len());
                consumed += 1;
            }
            None => {
                parse_errors += 1;
            }
        }
    }

    let final_backlog = queue.len();
    let processed_ratio = if produced == 0 {
        1.0
    } else {
        consumed as f64 / produced as f64
    };

    println!(
        "[soak] done: produced={}, consumed={}, ratio={:.3}, parse_errors={}, max_backlog={}, final_backlog={}, peak_frame_processed={}, max_frame_work_ms={:.3}",
        produced,
        consumed,
        processed_ratio,
        parse_errors,
        max_backlog,
        final_backlog,
        peak_processed_frame,
        max_frame_work.as_secs_f64() * 1000.0
    );

    assert_eq!(parse_errors, 0, "soak had parse/conversion failures");
    assert!(span_checksum > 0, "pipeline did not tokenize any spans");
    assert!(
        processed_ratio >= 0.95,
        "consumer fell behind too far (ratio={processed_ratio:.3})"
    );
    assert!(
        final_backlog <= msgs_per_sec * 3,
        "backlog remained too high at end ({} > {})",
        final_backlog,
        msgs_per_sec * 3
    );
}

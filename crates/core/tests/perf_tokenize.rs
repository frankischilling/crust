use std::time::{Duration, Instant};

use crust_core::format::{parse_twitch_emotes_tag, tokenize};
use crust_core::highlight::{is_highlighted, HighlightRule};

fn report(label: &str, ops: usize, elapsed: Duration) {
    let secs = elapsed.as_secs_f64();
    let ops_per_sec = if secs > 0.0 { ops as f64 / secs } else { 0.0 };
    println!(
        "[perf] {label}: {ops} ops in {:?} ({ops_per_sec:.0} ops/s)",
        elapsed
    );
}

#[test]
#[ignore]
fn perf_tokenize_mixed_chat_lines() {
    let text = "@mod check this clip https://clips.twitch.tv/FancyClip PogChamp Kappa 😀 let's go";
    let twitch_emotes = parse_twitch_emotes_tag("25:55-62");

    let emote_lookup = |word: &str| match word {
        "PogChamp" => Some((
            "88".to_owned(),
            "PogChamp".to_owned(),
            "https://example.local/pog.png".to_owned(),
            "twitch".to_owned(),
            Some("https://example.local/pog-4x.png".to_owned()),
        )),
        "Kappa" => Some((
            "25".to_owned(),
            "Kappa".to_owned(),
            "https://example.local/kappa.png".to_owned(),
            "twitch".to_owned(),
            Some("https://example.local/kappa-4x.png".to_owned()),
        )),
        _ => None,
    };

    for _ in 0..2_000 {
        let _ = tokenize(text, false, &twitch_emotes, &emote_lookup);
    }

    let iterations = 80_000usize;
    let start = Instant::now();
    let mut span_checksum = 0usize;
    for _ in 0..iterations {
        let spans = tokenize(text, false, &twitch_emotes, &emote_lookup);
        span_checksum = span_checksum.wrapping_add(spans.len());
    }
    let elapsed = start.elapsed();

    report("core::format::tokenize", iterations, elapsed);
    assert!(span_checksum > 0);
}

#[test]
#[ignore]
fn perf_parse_twitch_emotes_tag() {
    let tag = "25:0-4,12-16/1902:6-10/30259:18-23,30-35";

    for _ in 0..5_000 {
        let _ = parse_twitch_emotes_tag(tag);
    }

    let iterations = 300_000usize;
    let start = Instant::now();
    let mut checksum = 0usize;
    for _ in 0..iterations {
        let v = parse_twitch_emotes_tag(tag);
        checksum = checksum.wrapping_add(v.len());
    }
    let elapsed = start.elapsed();

    report("core::format::parse_twitch_emotes_tag", iterations, elapsed);
    assert!(checksum > 0);
}

#[test]
#[ignore]
fn perf_highlight_rule_matching() {
    let mut rules = Vec::new();
    for i in 0..64 {
        rules.push(HighlightRule::new(format!("keyword{i}")));
    }
    rules.push(HighlightRule::new("raid"));
    rules.push(HighlightRule::new("giveaway"));

    let msg = "big RAID incoming with giveaway winner announcement";

    for _ in 0..10_000 {
        let _ = is_highlighted(&rules, msg);
    }

    let iterations = 200_000usize;
    let start = Instant::now();
    let mut hits = 0usize;
    for _ in 0..iterations {
        if is_highlighted(&rules, msg) {
            hits += 1;
        }
    }
    let elapsed = start.elapsed();

    report("core::highlight::is_highlighted", iterations, elapsed);
    assert_eq!(hits, iterations);
}

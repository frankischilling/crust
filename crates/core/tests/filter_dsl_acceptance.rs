//! Integration acceptance tests for the Chatterino-style filter DSL (C3).
//!
//! These cover the exact acceptance criteria on the ticket:
//!  * `author.subscriber && message.content contains "gg"` parses, type-
//!    checks, evaluates to `true` for a subscriber GG line, and `false` for
//!    non-subscribers / non-GG lines.
//!  * An invalid expression yields a `ParseError` that carries position
//!    information (line, column, byte offsets).

use chrono::Utc;
use smallvec::smallvec;

use crust_core::filters::{
    build_message_context, evaluate, parse, synthesize_type, Type, Value,
    MESSAGE_TYPING_CONTEXT,
};
use crust_core::highlight::{
    compile_rules, first_match_context_rule_message, HighlightRule, HighlightRuleMode,
};
use crust_core::model::filters::{
    check_filters_message, compile_filters, FilterAction, FilterMode, FilterRecord, FilterScope,
};
use crust_core::model::{Badge, ChannelId, ChatMessage, MessageId, MsgKind, Sender, Span, UserId};

const TICKET_EXPR: &str = r#"author.subscriber && message.content contains "gg""#;

fn make_message(text: &str, subscriber: bool) -> ChatMessage {
    let mut badges = Vec::new();
    if subscriber {
        badges.push(Badge {
            name: "subscriber".into(),
            version: "6".into(),
            url: None,
        });
    }
    ChatMessage {
        id: MessageId(1),
        server_id: None,
        timestamp: Utc::now(),
        channel: ChannelId("acceptance".into()),
        sender: Sender {
            user_id: UserId("1".into()),
            login: "alice".into(),
            display_name: "Alice".into(),
            color: None,
            name_paint: None,
            badges,
        },
        raw_text: text.to_string(),
        spans: smallvec![Span::Text {
            text: text.to_string(),
            is_action: false,
        }],
        twitch_emotes: Vec::new(),
        flags: Default::default(),
        reply: None,
        msg_kind: MsgKind::Chat,
        shared: None,
    }
}

#[test]
fn ticket_expression_parses_and_type_checks_as_bool() {
    let ast = parse(TICKET_EXPR).expect("expression must parse");
    let ty = synthesize_type(&ast, &*MESSAGE_TYPING_CONTEXT)
        .expect("expression must type-check against the message context");
    assert_eq!(ty, Type::Bool, "ticket expression must be Bool-typed");
}

#[test]
fn ticket_expression_fires_for_subscriber_gg() {
    let ast = parse(TICKET_EXPR).unwrap();
    let msg = make_message("gg ez", true);
    let ctx = build_message_context(&msg, "acceptance", Some(true), false);
    let v = evaluate(&ast, &ctx);
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn ticket_expression_rejects_non_subscriber() {
    let ast = parse(TICKET_EXPR).unwrap();
    let msg = make_message("gg ez", false);
    let ctx = build_message_context(&msg, "acceptance", None, false);
    assert_eq!(evaluate(&ast, &ctx), Value::Bool(false));
}

#[test]
fn ticket_expression_rejects_subscriber_without_gg() {
    let ast = parse(TICKET_EXPR).unwrap();
    let msg = make_message("nice play!", true);
    let ctx = build_message_context(&msg, "acceptance", None, false);
    assert_eq!(evaluate(&ast, &ctx), Value::Bool(false));
}

#[test]
fn ticket_expression_end_to_end_via_filter_record() {
    let mut rec = FilterRecord::new("sub_gg", TICKET_EXPR, FilterScope::Global);
    rec.mode = FilterMode::Expression;
    rec.action = FilterAction::Hide;
    let compiled = compile_filters(&[rec]);
    assert_eq!(compiled.len(), 1);

    let sub_gg = make_message("gg ez win", true);
    let r = check_filters_message(&compiled, None, &sub_gg, "acceptance", Some(true), false);
    assert_eq!(r, Some(FilterAction::Hide));

    let non_sub = make_message("gg ez win", false);
    assert_eq!(
        check_filters_message(&compiled, None, &non_sub, "acceptance", None, false),
        None,
    );

    let sub_other = make_message("wow what a play", true);
    assert_eq!(
        check_filters_message(&compiled, None, &sub_other, "acceptance", None, false),
        None,
    );
}

#[test]
fn ticket_expression_end_to_end_via_highlight_rule() {
    let rule = HighlightRule::expression(TICKET_EXPR);
    assert_eq!(rule.mode, HighlightRuleMode::Expression);
    let compiled = compile_rules(&[rule]);

    let sub_gg = make_message("gg ez", true);
    let hit = first_match_context_rule_message(&compiled, &sub_gg, "acceptance", Some(true), false);
    assert!(hit.is_some(), "subscriber GG message should match highlight");

    let non_sub = make_message("gg ez", false);
    assert!(
        first_match_context_rule_message(&compiled, &non_sub, "acceptance", None, false).is_none(),
        "non-subscriber must not match"
    );
}

#[test]
fn invalid_expression_returns_parse_error_with_position() {
    // Trailing binary operator with nothing on the right-hand side.
    let err = parse("author.subscriber &&").unwrap_err();
    let sp = err.span();
    // Span must point somewhere inside the input, not past EOF.
    assert!(
        sp.start <= "author.subscriber &&".len(),
        "error span start {} out of range",
        sp.start
    );
    assert!(sp.end >= sp.start, "span end before start");
    assert!(sp.line >= 1, "line number must be 1-based");
    assert!(sp.col >= 1, "column number must be 1-based");
    let msg = err.to_string();
    assert!(
        !msg.is_empty(),
        "parse error must carry a human-readable message"
    );
}

#[test]
fn invalid_expression_on_second_line_reports_correct_line_column() {
    // Force a line/column other than (1,1) so we know the lexer tracks lines.
    let src = "author.subscriber\n&& 123 456";
    let err = parse(src).unwrap_err();
    let sp = err.span();
    assert!(
        sp.line >= 2,
        "expected error to be reported on line 2+, got line {}",
        sp.line
    );
}

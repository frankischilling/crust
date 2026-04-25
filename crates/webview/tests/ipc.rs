use crust_webview::ipc::IncomingMessage;

#[test]
fn parses_login_true() {
    let m = IncomingMessage::parse(r#"{"kind":"login","logged_in":true}"#).unwrap();
    assert!(matches!(m, IncomingMessage::Login { logged_in: true }));
}

#[test]
fn parses_login_false() {
    let m = IncomingMessage::parse(r#"{"kind":"login","logged_in":false}"#).unwrap();
    assert!(matches!(m, IncomingMessage::Login { logged_in: false }));
}

#[test]
fn parses_claimed() {
    let m = IncomingMessage::parse(r#"{"kind":"claimed"}"#).unwrap();
    assert!(matches!(m, IncomingMessage::Claimed));
}

#[test]
fn parses_balance() {
    let m = IncomingMessage::parse(r#"{"kind":"balance","value":1234}"#).unwrap();
    assert!(matches!(m, IncomingMessage::Balance { value: 1234 }));
}

#[test]
fn parses_error_as_error() {
    let m = IncomingMessage::parse(
        r#"{"kind":"error","where":"claim","msg":"x is undefined"}"#,
    ).unwrap();
    match m {
        IncomingMessage::Error { location, message } => {
            assert_eq!(location, "claim");
            assert_eq!(message, "x is undefined");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn rejects_unknown_kind() {
    assert!(IncomingMessage::parse(r#"{"kind":"wat"}"#).is_err());
}

#[test]
fn rejects_garbage() {
    assert!(IncomingMessage::parse("not json").is_err());
}

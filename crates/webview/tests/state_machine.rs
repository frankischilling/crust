use crust_webview::state::{LoginState, WebviewState};

#[test]
fn initial_state_is_unconfigured() {
    let s = WebviewState::new();
    assert_eq!(s.login_state(), LoginState::Unknown);
    assert_eq!(s.active_channel(), None);
    assert!(!s.is_visible());
}

#[test]
fn set_login_detected_transitions_to_logged_in_and_hides_window() {
    let mut s = WebviewState::new();
    s.open_login_window(); // user clicked "Open Twitch sign-in"
    assert!(s.is_visible());
    s.set_login_detected(true);
    assert_eq!(s.login_state(), LoginState::LoggedIn);
    assert!(!s.is_visible(), "window hides once auth-token is seen");
}

#[test]
fn set_login_detected_false_after_logout_requires_user_action_to_show() {
    let mut s = WebviewState::new();
    s.set_login_detected(true);
    s.set_login_detected(false);
    assert_eq!(s.login_state(), LoginState::LoggedOut);
    assert!(!s.is_visible(), "we do not pop a window unsolicited");
}

#[test]
fn set_active_channel_only_stored_when_logged_in() {
    let mut s = WebviewState::new();
    s.set_active_channel(Some("xqc".into()));
    assert_eq!(s.active_channel(), None, "ignored while not logged in");
    s.set_login_detected(true);
    s.set_active_channel(Some("xqc".into()));
    assert_eq!(s.active_channel(), Some("xqc"));
}

#[test]
fn open_login_window_works_after_logout() {
    let mut s = WebviewState::new();
    s.set_login_detected(true);
    s.set_login_detected(false);
    s.open_login_window();
    assert!(s.is_visible(), "user must be able to re-sign-in after a logout");
}

#[test]
fn active_channel_cleared_on_logout() {
    let mut s = WebviewState::new();
    s.set_login_detected(true);
    s.set_active_channel(Some("xqc".into()));
    assert_eq!(s.active_channel(), Some("xqc"));
    s.set_login_detected(false);
    assert_eq!(s.active_channel(), None, "logout clears focused channel");
}

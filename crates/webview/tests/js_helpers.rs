use crust_webview::js;

#[test]
fn login_probe_script_reads_auth_token_cookie() {
    let script = js::LOGIN_PROBE;
    assert!(script.contains("document.cookie"));
    assert!(script.contains("auth-token"));
    assert!(script.contains("window.ipc"));
}

#[test]
fn claim_click_script_targets_community_points_button() {
    let script = js::CLAIM_CLICK;
    // Selector chain must cover both the "claim bonus" badge and the
    // pre-redesign classname, since Twitch A/B-tests this surface.
    assert!(script.contains("community-points-summary"));
    assert!(script.contains("claimable-bonus"));
    assert!(script.contains(".click()"));
}

#[test]
fn balance_probe_script_returns_numeric_only() {
    let script = js::BALANCE_PROBE;
    // Must strip commas/spaces before Number() so "1,234" -> 1234.
    assert!(script.contains("replace"));
    assert!(script.contains("Number"));
}

#[test]
fn channel_url_is_main_channel_page_lowercased() {
    // Sidecar navigates to the main channel page - that's where the
    // `.community-points-summary` widget lives. Stream audio/video is
    // silenced by MUTE_BOOTSTRAP's `.remove()` pass.
    assert_eq!(js::channel_url("xQc"), "https://www.twitch.tv/xqc");
    assert_eq!(
        js::channel_url("AgendaFreeTV"),
        "https://www.twitch.tv/agendafreetv"
    );
}

#[test]
fn mute_bootstrap_blocks_hls_network_fetches() {
    // The only reliable way to silence the stream is to prevent the
    // player from ever getting media bytes. Bootstrap must wrap fetch
    // and XHR and reject Twitch's HLS endpoints.
    let script = js::MUTE_BOOTSTRAP;
    assert!(script.contains("usher"));
    assert!(script.contains("video-weaver"));
    assert!(script.contains(".m3u8"));
    assert!(script.contains("window.fetch"));
    assert!(script.contains("XMLHttpRequest"));
    assert!(script.contains(".remove()"));
    assert!(script.contains("MutationObserver"));
}

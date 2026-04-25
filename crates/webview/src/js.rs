//! JavaScript strings injected into the Twitch page.
//!
//! Every snippet calls `window.ipc.postMessage(JSON.stringify(...))` - that's
//! the wry-provided bridge that routes back into [`crate::ipc`]. Keep each
//! script small and selector-defensive: Twitch rotates classnames on a ~6
//! month cadence and we absorb that with defensive chains rather than a
//! single brittle query.

/// Injected at document-start on every navigation, before Twitch's own
/// scripts run. The main channel page auto-plays stream audio+video,
/// and merely muting the `<video>` element loses a race with React's
/// re-renders and doesn't intercept Web-Audio routed streams. The
/// only reliable fix is to prevent the stream bytes from ever reaching
/// the player - we block HLS manifest + segment fetches at the
/// network layer.
///
/// Defense layers (in order of strength):
///
/// 1. **Block HLS network requests.** Twitch's HLS manifest comes from
///    `usher.ttvnw.net`; segments from `video-weaver.*`/`video-edge.*`
///    CDNs, or any `.m3u8`/`.ts` URL. We wrap `fetch` and
///    `XMLHttpRequest` to reject matching URLs. No manifest -> no
///    segments -> no audio bytes, regardless of player state.
/// 2. **Stub AudioContext.** Belt-and-suspenders in case Twitch ever
///    generates audio client-side.
/// 3. **Remove `<video>`/`<audio>` elements.** MutationObserver +
///    periodic sweep so any that slip through get torn out of the DOM.
pub const MUTE_BOOTSTRAP: &str = r#"
(function() {
  function isMedia(url) {
    if (!url) return false;
    var u = String(url);
    return /usher\.ttvnw\.net|video-weaver|video-edge|\.m3u8(\?|$)|\.ts(\?|$)|hls\.ttvnw\.net/i.test(u);
  }
  // Layer 1: block HLS fetches
  try {
    var origFetch = window.fetch;
    window.fetch = function(input, init) {
      var url = typeof input === "string" ? input : (input && input.url);
      if (isMedia(url)) {
        return Promise.reject(new TypeError("crust: blocked media fetch"));
      }
      return origFetch.apply(this, arguments);
    };
  } catch (e) {}
  try {
    var XHR = XMLHttpRequest.prototype;
    var origOpen = XHR.open;
    var origSend = XHR.send;
    XHR.open = function(method, url) {
      try { this.__crustBlocked = isMedia(url); } catch (e) {}
      return origOpen.apply(this, arguments);
    };
    XHR.send = function() {
      if (this.__crustBlocked) {
        var self = this;
        setTimeout(function() {
          try { self.dispatchEvent(new Event("error")); } catch (e) {}
        }, 0);
        return;
      }
      return origSend.apply(this, arguments);
    };
  } catch (e) {}
  // Layer 2: stub Web Audio
  try {
    function FakeAudioContext() {
      return {
        createGain: function() { return { gain: { value: 0 }, connect: function(){}, disconnect: function(){} }; },
        createMediaElementSource: function() { return { connect: function(){}, disconnect: function(){} }; },
        createMediaStreamSource: function() { return { connect: function(){}, disconnect: function(){} }; },
        createBufferSource: function() { return { buffer: null, connect: function(){}, disconnect: function(){}, start: function(){}, stop: function(){} }; },
        destination: { connect: function(){}, disconnect: function(){} },
        resume: function() { return Promise.resolve(); },
        suspend: function() { return Promise.resolve(); },
        close: function() { return Promise.resolve(); },
        state: "suspended"
      };
    }
    try { window.AudioContext = FakeAudioContext; } catch (e) {}
    try { window.webkitAudioContext = FakeAudioContext; } catch (e) {}
  } catch (e) {}
  // Layer 3: remove media elements on sight
  function kill() {
    try {
      document.querySelectorAll("video, audio").forEach(function(el) {
        try {
          try { el.pause(); } catch (e) {}
          try { el.muted = true; } catch (e) {}
          try { el.volume = 0; } catch (e) {}
          try { el.src = ""; } catch (e) {}
          try { el.srcObject = null; } catch (e) {}
          try { el.removeAttribute("src"); } catch (e) {}
          try { el.load(); } catch (e) {}
          try { el.remove(); } catch (e) {}
        } catch (e) {}
      });
    } catch (e) {}
  }
  kill();
  try {
    var obs = new MutationObserver(kill);
    obs.observe(document.documentElement || document.body || document, {
      childList: true,
      subtree: true,
    });
  } catch (e) {}
  setInterval(kill, 250);
})();
"#;

/// Run on every tick to report whether the viewer is authenticated.
pub const LOGIN_PROBE: &str = r#"
(function() {
  try {
    var ck = document.cookie || "";
    var hasAuth = /(?:^|;\s*)auth-token=([A-Za-z0-9]+)/.test(ck);
    window.ipc.postMessage(JSON.stringify({ kind: "login", logged_in: hasAuth }));
  } catch (e) {
    window.ipc.postMessage(JSON.stringify({ kind: "error", where: "login", msg: String(e) }));
  }
})();
"#;

/// Run on every tick to click the bonus-points button if it is visible.
/// Safe to run when nothing is present - becomes a no-op.
pub const CLAIM_CLICK: &str = r#"
(function() {
  try {
    var btn =
      document.querySelector('[data-test-selector="community-points-summary__bonus-icon-button"]')
      || document.querySelector('.community-points-summary .claimable-bonus__icon')
      || document.querySelector('.community-points-summary button[aria-label*="Claim Bonus"]');
    if (btn) {
      btn.click();
      window.ipc.postMessage(JSON.stringify({ kind: "claimed" }));
    }
  } catch (e) {
    window.ipc.postMessage(JSON.stringify({ kind: "error", where: "claim", msg: String(e) }));
  }
})();
"#;

/// Run on every tick to report the current balance. Strips non-digit chars so
/// "1,234" or "2.3K" parses correctly (the UI uses compact notation above 1K).
pub const BALANCE_PROBE: &str = r#"
(function() {
  try {
    var el =
      document.querySelector('[data-test-selector="balance-string"]')
      || document.querySelector('.community-points-summary span');
    if (!el) return;
    var txt = (el.textContent || "").replace(/[,\s]/g, "");
    var k = /k$/i.test(txt), m = /m$/i.test(txt);
    var n = Number(txt.replace(/[^\d.]/g, ""));
    if (!isFinite(n)) return;
    if (k) n *= 1000;
    if (m) n *= 1000000;
    window.ipc.postMessage(JSON.stringify({ kind: "balance", value: Math.round(n) }));
  } catch (e) {
    window.ipc.postMessage(JSON.stringify({ kind: "error", where: "balance", msg: String(e) }));
  }
})();
"#;

/// Build the channel URL the sidecar navigates to. We use the main
/// channel page rather than the chat-popout because the popout does not
/// render the `.community-points-summary` widget - the bonus button and
/// balance selectors only exist on the main channel page.
///
/// Stream audio/video is handled by `MUTE_BOOTSTRAP`, which mutes and
/// then `.remove()`s every `<video>`/`<audio>` element on the page, so
/// no stream audio leaks out of the hidden sidecar window and no HLS
/// bandwidth is consumed.
///
/// Twitch lowercases logins in these routes; mixed-case paths 301 to
/// the lowercase one, so we normalise.
pub fn channel_url(login: &str) -> String {
    format!("https://www.twitch.tv/{}", login.to_ascii_lowercase())
}

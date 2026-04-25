//! Pure state machine for the Twitch webview bridge. No I/O, no wry types -
//! so it can be unit-tested without a display.

/// Whether we've observed an `auth-token` cookie in the webview's session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginState {
    /// No cookie probe has returned yet.
    Unknown,
    /// `auth-token` cookie present and non-empty.
    LoggedIn,
    /// Cookie probe returned but `auth-token` was missing/empty.
    LoggedOut,
}

/// Everything the runtime thread needs to know between commands and ticks.
///
/// Invariants:
/// - `active_channel.is_some()` implies `login == LoginState::LoggedIn`.
///   `set_login_detected(false)` clears `active_channel` to preserve this.
/// - `visible == true` means the sign-in window is on-screen. It may only
///   be set true by an explicit `open_login_window` call, never by the
///   probe.
#[derive(Debug)]
pub struct WebviewState {
    login: LoginState,
    active_channel: Option<String>,
    visible: bool,
}

impl WebviewState {
    pub fn new() -> Self {
        Self {
            login: LoginState::Unknown,
            active_channel: None,
            visible: false,
        }
    }

    pub fn login_state(&self) -> LoginState {
        self.login
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn active_channel(&self) -> Option<&str> {
        self.active_channel.as_deref()
    }

    /// User explicitly asked to sign in. Shows the window until login lands.
    pub fn open_login_window(&mut self) {
        self.visible = true;
    }

    /// Cookie probe result. Transition:
    /// - true  -> LoggedIn, hide the window (login just completed)
    /// - false -> LoggedOut, clear active_channel, keep current visibility
    ///           (do not pop the window unsolicited)
    pub fn set_login_detected(&mut self, detected: bool) {
        if detected {
            self.login = LoginState::LoggedIn;
            self.visible = false;
        } else {
            self.login = LoginState::LoggedOut;
            self.active_channel = None;
        }
    }

    /// Focus changed in the main app. Only honored once login is established;
    /// pre-login navigations would just get redirected to Twitch's sign-in
    /// page anyway.
    pub fn set_active_channel(&mut self, channel_login: Option<String>) {
        if self.login == LoginState::LoggedIn {
            self.active_channel = channel_login;
        }
    }
}

impl Default for WebviewState {
    fn default() -> Self {
        Self::new()
    }
}

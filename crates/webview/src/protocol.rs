//! Wire format for the parent ↔ sidecar JSON-lines protocol.
//!
//! Each direction writes one compact JSON object per line to the stream.
//! Parent -> child goes over the child's stdin; child -> parent over stdout.
//! Backward-compatible tagging: every record has a `kind` field for
//! dispatch, plus variant-specific fields. Unknown kinds are dropped on
//! the receiver with a trace log, never fatal.
//!
//! This module is pure serde - no I/O, no threads - so both the library
//! (parent side) and the `crust-webview` host binary (child side) depend
//! on it.

use serde::{Deserialize, Serialize};

use crate::state::LoginState;

/// Commands flowing parent -> child.
///
/// Serialised as `{"kind":"...", ...}` so new variants can be added without
/// breaking older sidecar binaries (they just `trace!` and skip unknowns).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostCommand {
    /// Show the webview window so the user can sign in.
    OpenLogin,
    /// Focus changed to the given Twitch channel login. `None` pauses
    /// navigation - the child may keep the current tab on-screen but the
    /// claim/balance probes no-op.
    SetActiveChannel {
        #[serde(default)]
        login: Option<String>,
    },
    /// Master enable/disable. Mirrors the UI's auto-claim checkbox.
    SetEnabled { enabled: bool },
    /// Graceful shutdown. Child finishes any in-flight work and exits 0.
    Shutdown,
}

/// Events flowing child -> parent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostEvent {
    /// Login state transitioned. Emitted once per transition, not per tick.
    LoginState { state: LoginStateWire },
    /// A bonus-points button was DOM-clicked this tick.
    Claimed,
    /// DOM balance scraped for the focused channel.
    Balance { value: u64 },
    /// Non-fatal JS error from an injected script.
    ScriptError { location: String, message: String },
    /// The host exited its event loop. Informational - the process is about
    /// to terminate.
    Exited,
}

/// Wire form of [`crate::state::LoginState`]. Using a separate enum so we
/// control the JSON representation independently of the in-process type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoginStateWire {
    Unknown,
    LoggedIn,
    LoggedOut,
}

impl From<LoginState> for LoginStateWire {
    fn from(s: LoginState) -> Self {
        match s {
            LoginState::Unknown => LoginStateWire::Unknown,
            LoginState::LoggedIn => LoginStateWire::LoggedIn,
            LoginState::LoggedOut => LoginStateWire::LoggedOut,
        }
    }
}

impl From<LoginStateWire> for LoginState {
    fn from(s: LoginStateWire) -> Self {
        match s {
            LoginStateWire::Unknown => LoginState::Unknown,
            LoginStateWire::LoggedIn => LoginState::LoggedIn,
            LoginStateWire::LoggedOut => LoginState::LoggedOut,
        }
    }
}

/// Serialise a command to a single JSON line (no trailing newline).
pub fn encode_command(cmd: &HostCommand) -> String {
    // Unwrap: our enum is infallibly serialisable; fall back to a safe
    // empty-shutdown on the (impossible) failure path rather than panicking.
    serde_json::to_string(cmd).unwrap_or_else(|_| r#"{"kind":"shutdown"}"#.to_owned())
}

/// Serialise an event to a single JSON line (no trailing newline).
pub fn encode_event(evt: &HostEvent) -> String {
    serde_json::to_string(evt).unwrap_or_else(|_| r#"{"kind":"exited"}"#.to_owned())
}

/// Parse one JSON line from the child.
pub fn decode_event(line: &str) -> Result<HostEvent, serde_json::Error> {
    serde_json::from_str(line)
}

/// Parse one JSON line from the parent.
pub fn decode_command(line: &str) -> Result<HostCommand, serde_json::Error> {
    serde_json::from_str(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_command_roundtrip() {
        let c = HostCommand::SetActiveChannel {
            login: Some("xqc".into()),
        };
        let line = encode_command(&c);
        assert!(line.contains(r#""kind":"set_active_channel""#));
        assert!(line.contains(r#""login":"xqc""#));
        let decoded = decode_command(&line).unwrap();
        match decoded {
            HostCommand::SetActiveChannel { login } => assert_eq!(login.as_deref(), Some("xqc")),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn encode_command_null_channel() {
        let c = HostCommand::SetActiveChannel { login: None };
        let line = encode_command(&c);
        let decoded = decode_command(&line).unwrap();
        match decoded {
            HostCommand::SetActiveChannel { login } => assert!(login.is_none()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn encode_event_login_logged_in() {
        let e = HostEvent::LoginState {
            state: LoginStateWire::LoggedIn,
        };
        let line = encode_event(&e);
        assert!(line.contains(r#""kind":"login_state""#));
        assert!(line.contains(r#""logged_in""#));
        let decoded = decode_event(&line).unwrap();
        match decoded {
            HostEvent::LoginState { state } => assert_eq!(state, LoginStateWire::LoggedIn),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn decode_command_all_kinds() {
        assert!(matches!(
            decode_command(r#"{"kind":"open_login"}"#).unwrap(),
            HostCommand::OpenLogin
        ));
        assert!(matches!(
            decode_command(r#"{"kind":"shutdown"}"#).unwrap(),
            HostCommand::Shutdown
        ));
        assert!(matches!(
            decode_command(r#"{"kind":"set_enabled","enabled":true}"#).unwrap(),
            HostCommand::SetEnabled { enabled: true }
        ));
    }

    #[test]
    fn decode_command_unknown_kind_errors() {
        assert!(decode_command(r#"{"kind":"frobnicate"}"#).is_err());
    }

    #[test]
    fn login_state_wire_roundtrip() {
        for s in [
            LoginState::Unknown,
            LoginState::LoggedIn,
            LoginState::LoggedOut,
        ] {
            let wire: LoginStateWire = s.into();
            let back: LoginState = wire.into();
            assert_eq!(s, back);
        }
    }
}

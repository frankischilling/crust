//! Parent-side control of the embedded Twitch auto-claim sidecar.
//!
//! This crate is pure-Rust (no wry, no tao, no WebView2 link). It spawns
//! a `crust-webview-host` sibling binary that owns the real WebView in its
//! own process, isolating the main Crust process from COM / winit thread-
//! affinity crashes that occur when a WebView2 instance is driven from a
//! non-main thread inside an app that already has another event loop.
//!
//! See `docs/superpowers/specs/2026-04-24-twitch-webview-auto-claim-setup.md`
//! for the user-facing setup flow and rationale for the sidecar split.

pub mod ipc;
pub mod js;
pub mod protocol;
pub mod runtime;
pub mod state;

pub use ipc::IncomingMessage;
pub use protocol::{HostCommand, HostEvent, LoginStateWire};
pub use runtime::{spawn, WebviewCommand, WebviewEvent, WebviewHandle};
pub use state::{LoginState, WebviewState};

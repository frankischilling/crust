//! Parent-side controller for the `crust-webview` sidecar binary.
//!
//! The embedded WebView2 lives in its own process - `crust-webview.exe` (a
//! separate workspace binary) - to sidestep COM / winit thread-affinity
//! crashes that were surfacing as `STATUS_HEAP_CORRUPTION` when the WebView
//! ran on a secondary thread of the main Crust process.
//!
//! This module:
//! 1. Spawns the sidecar, passing the persistent `data_dir` as its sole CLI
//!    argument, with stdio piped.
//! 2. Runs two OS threads:
//!    - **stdin pump** - serialises [`WebviewCommand`]s to JSON lines and
//!      writes them to the child's stdin.
//!    - **stdout reader** - deserialises one JSON line at a time from the
//!      child's stdout into [`WebviewEvent`]s and forwards them to the
//!      caller's [`mpsc::Sender`].
//! 3. Exposes a [`WebviewHandle`] whose [`Drop`] impl sends `Shutdown` and
//!    closes the stdin pipe - the child then exits its event loop.
//!
//! If the sidecar binary is missing or cannot be spawned, [`spawn`] returns
//! a no-op handle that silently drops every command. This lets Crust run
//! without auto-claim on systems lacking `crust-webview.exe` (e.g. a dev
//! build that only compiled `crust`).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::protocol::{decode_event, encode_command, HostCommand, HostEvent};
use crate::state::LoginState;

// Public API

/// Commands sent from the reducer into the webview sidecar.
#[derive(Debug, Clone)]
pub enum WebviewCommand {
    /// Show the sidecar's browser window so the user can sign in to twitch.tv.
    OpenLoginWindow,
    /// Update the focused Twitch channel. Triggers navigation + probes when
    /// logged in; ignored otherwise.
    SetActiveChannel(Option<String>),
    /// Master toggle - mirrors the "Auto-claim Bonus Points" UI checkbox.
    SetEnabled(bool),
    /// Graceful shutdown. Sent automatically by `WebviewHandle::drop`.
    ///
    /// ⚠️ Do not dispatch from app code other than the drop path - the
    /// sidecar terminates its process on receipt. Use `SetEnabled(false)`
    /// to pause auto-claim mid-session.
    Shutdown,
}

/// Events emitted by the sidecar back to the reducer.
#[derive(Debug, Clone)]
pub enum WebviewEvent {
    /// Login state transitioned in the sidecar's WebView session.
    LoginStateChanged(LoginState),
    /// The bonus-points button was DOM-clicked this tick.
    BonusClaimed,
    /// Balance scraped from the DOM for the focused channel.
    BalanceObserved(u64),
    /// Non-fatal script-error report from the sidecar.
    ScriptError { location: String, message: String },
}

/// Handle returned by [`spawn`]. `Drop` sends `Shutdown` on a best-effort
/// basis.
pub struct WebviewHandle {
    cmd_tx: Option<mpsc::Sender<WebviewCommand>>,
    // Kept alive so the child process isn't orphaned. We don't actively
    // `wait()` on it - the stdin pump closes the pipe on Drop, the child
    // sees EOF and exits.
    _child: Option<ChildGuard>,
}

impl WebviewHandle {
    pub fn send(&self, cmd: WebviewCommand) {
        let Some(tx) = &self.cmd_tx else { return };
        if let Err(e) = tx.try_send(cmd) {
            debug!("crust-webview: command dropped: {e}");
        }
    }
}

impl Drop for WebviewHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.cmd_tx.take() {
            // try_send so we never block app teardown on a backed-up pump.
            let _ = tx.try_send(WebviewCommand::Shutdown);
            // Drop tx -> pump thread sees `recv()` return None -> closes
            // stdin -> child sees EOF -> exits cleanly.
        }
        // _child (if present) drops here. ChildGuard::drop waits briefly
        // for graceful exit, then kills the process if it's still alive.
    }
}

// Child-process guard

/// Owns the child process handle and ensures it exits when this struct
/// drops. We try polite shutdown first (via stdin close), then kill after
/// a short grace period.
struct ChildGuard {
    child: Child,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_millis(2_000);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => return,
            }
        }
    }
}

// spawn

/// Launch the sidecar binary and wire its stdio to the caller's mpsc
/// channels.
///
/// `data_dir` is the persistent user-data directory (WebView2 profile) -
/// typically `<config_dir>/webview`. The sidecar creates it if it doesn't
/// exist.
///
/// On any failure (binary not found, spawn error, pipe setup error) this
/// returns a handle that silently ignores every command. The log will say
/// why in one line.
pub fn spawn(
    data_dir: PathBuf,
    evt_tx: mpsc::Sender<WebviewEvent>,
) -> WebviewHandle {
    let Some(binary) = locate_sidecar() else {
        warn!(
            "crust-webview: sidecar binary `crust-webview[.exe]` not found next \
             to the main executable. Build with `cargo build --release --workspace` \
             to produce it. Auto-claim is disabled for this session."
        );
        return dead_handle();
    };

    info!("crust-webview: spawning sidecar at {binary:?}");

    let child_result = Command::new(&binary)
        .arg(&data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // stderr inherits so the sidecar's tracing output shows in the
        // Crust console alongside main-process logs.
        .stderr(Stdio::inherit())
        .spawn();
    let mut child = match child_result {
        Ok(c) => c,
        Err(e) => {
            warn!("crust-webview: failed to spawn {binary:?}: {e}");
            return dead_handle();
        }
    };

    let stdin = match child.stdin.take() {
        Some(s) => s,
        None => {
            warn!("crust-webview: child had no stdin pipe");
            let _ = child.kill();
            return dead_handle();
        }
    };
    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            warn!("crust-webview: child had no stdout pipe");
            let _ = child.kill();
            return dead_handle();
        }
    };

    let (cmd_tx, cmd_rx) = mpsc::channel::<WebviewCommand>(16);

    // Stdin pump: serialize commands -> JSON -> writeln.
    std::thread::Builder::new()
        .name("crust-webview-stdin".into())
        .spawn(move || stdin_pump(stdin, cmd_rx))
        .expect("spawn stdin pump");

    // Stdout reader: parse JSON lines -> forward as WebviewEvent.
    std::thread::Builder::new()
        .name("crust-webview-stdout".into())
        .spawn(move || stdout_reader(stdout, evt_tx))
        .expect("spawn stdout reader");

    WebviewHandle {
        cmd_tx: Some(cmd_tx),
        _child: Some(ChildGuard { child }),
    }
}

fn dead_handle() -> WebviewHandle {
    WebviewHandle {
        cmd_tx: None,
        _child: None,
    }
}

/// Locate `crust-webview.exe` (or `crust-webview` on Unix) alongside the
/// currently-running binary. Returns `None` if we can't find it.
fn locate_sidecar() -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    let parent = current.parent()?.to_path_buf();
    let name = if cfg!(windows) {
        "crust-webview.exe"
    } else {
        "crust-webview"
    };
    let candidate = parent.join(name);
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

// Pumps

fn stdin_pump(stdin: std::process::ChildStdin, mut cmd_rx: mpsc::Receiver<WebviewCommand>) {
    let mut stdin = stdin;
    while let Some(cmd) = cmd_rx.blocking_recv() {
        let host_cmd = to_host_command(&cmd);
        let line = encode_command(&host_cmd);
        if writeln!(stdin, "{line}").is_err() {
            debug!("crust-webview: stdin closed; ending pump");
            break;
        }
        if matches!(cmd, WebviewCommand::Shutdown) {
            break;
        }
    }
}

fn stdout_reader(stdout: std::process::ChildStdout, evt_tx: mpsc::Sender<WebviewEvent>) {
    let reader = BufReader::new(stdout);
    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                debug!("crust-webview: stdout read error: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match decode_event(&line) {
            Ok(event) => {
                if let Some(mapped) = from_host_event(event) {
                    let _ = evt_tx.blocking_send(mapped);
                }
            }
            Err(e) => {
                debug!("crust-webview: bad event line: {e}; raw: {line}");
            }
        }
    }
    debug!("crust-webview: stdout reader exiting");
}

// Mappers

fn to_host_command(cmd: &WebviewCommand) -> HostCommand {
    match cmd {
        WebviewCommand::OpenLoginWindow => HostCommand::OpenLogin,
        WebviewCommand::SetActiveChannel(login) => HostCommand::SetActiveChannel {
            login: login.clone(),
        },
        WebviewCommand::SetEnabled(flag) => HostCommand::SetEnabled { enabled: *flag },
        WebviewCommand::Shutdown => HostCommand::Shutdown,
    }
}

fn from_host_event(evt: HostEvent) -> Option<WebviewEvent> {
    match evt {
        HostEvent::LoginState { state } => {
            let mapped: LoginState = state.into();
            Some(WebviewEvent::LoginStateChanged(mapped))
        }
        HostEvent::Claimed => Some(WebviewEvent::BonusClaimed),
        HostEvent::Balance { value } => Some(WebviewEvent::BalanceObserved(value)),
        HostEvent::ScriptError { location, message } => {
            Some(WebviewEvent::ScriptError { location, message })
        }
        HostEvent::Exited => None,
    }
}

// Unused but worth documenting

#[allow(dead_code)]
fn _silence_unused_lints(_p: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::LoginStateWire;

    #[test]
    fn dead_handle_ignores_commands_silently() {
        let h = dead_handle();
        // Should not panic, should not log at warn level, should just return.
        h.send(WebviewCommand::OpenLoginWindow);
        h.send(WebviewCommand::SetActiveChannel(Some("x".into())));
        h.send(WebviewCommand::SetEnabled(true));
    }

    #[test]
    fn to_host_command_preserves_fields() {
        match to_host_command(&WebviewCommand::SetActiveChannel(Some("xQc".into()))) {
            HostCommand::SetActiveChannel { login } => {
                assert_eq!(login.as_deref(), Some("xQc"));
            }
            _ => panic!("wrong variant"),
        }
        match to_host_command(&WebviewCommand::SetEnabled(true)) {
            HostCommand::SetEnabled { enabled } => assert!(enabled),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn from_host_event_exited_is_none() {
        assert!(from_host_event(HostEvent::Exited).is_none());
    }

    #[test]
    fn from_host_event_login_maps_wire() {
        match from_host_event(HostEvent::LoginState {
            state: LoginStateWire::LoggedIn,
        }) {
            Some(WebviewEvent::LoginStateChanged(LoginState::LoggedIn)) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }
}

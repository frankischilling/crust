//! `crust-webview` - sidecar binary owning the embedded system WebView.
//!
//! Runs in its own process, spawned by the main `crust.exe`. Reads JSON-
//! line [`crust_webview::HostCommand`]s from stdin, writes JSON-line
//! [`crust_webview::HostEvent`]s to stdout. Tracing logs go to stderr so
//! they interleave with the parent's output.
//!
//! The wry `WebView` + tao `EventLoop` live on this process's **main**
//! thread, which sidesteps the COM apartment / winit thread-affinity
//! issues we hit when running the WebView on a secondary thread of the
//! main Crust process.
//!
//! Usage (expected to be spawned by the parent, not run manually):
//!
//! ```text
//! crust-webview.exe <data-directory>
//! crust-webview.exe --smoke-test <temporary-data-directory>
//! ```
//!
//! The `<data-directory>` arg is the persistent WebView user-data dir
//! (cookies, IndexedDB, cache). The parent creates it beforehand.
//! The smoke test loads local HTML and exits after a JavaScript IPC reply.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use crust_webview::protocol::{
    decode_command, encode_event, HostCommand, HostEvent, LoginStateWire,
};
use crust_webview::state::{LoginState, WebviewState};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use tracing::{debug, info, warn};
use wry::{WebContext, WebViewBuilder};

/// Period between JS probe injections. 30 s matches Twitch's own analytics
/// heartbeat cadence closely enough to avoid detection while keeping the
/// user-visible latency of bonus-click to a known bound.
const TICK_PERIOD: Duration = Duration::from_secs(30);

const SMOKE_TEST_TIMEOUT: Duration = Duration::from_secs(20);
const SMOKE_TEST_READY: &str = "crust-webview-smoke-ready";
const SMOKE_TEST_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline'">
<title>Crust WebView smoke test</title></head><body>
<script>window.ipc.postMessage("crust-webview-smoke-ready");</script>
</body></html>"#;

#[derive(Debug, PartialEq, Eq)]
struct StartupOptions {
    data_dir: PathBuf,
    smoke_test: bool,
}

impl StartupOptions {
    fn parse(mut args: impl Iterator<Item = OsString>) -> Option<Self> {
        let first = args.next()?;
        let smoke_test = first == "--smoke-test";
        let data_dir = if smoke_test { args.next()? } else { first };
        if data_dir.is_empty()
            || data_dir.to_string_lossy().starts_with("--")
            || args.next().is_some()
        {
            return None;
        }
        Some(Self {
            data_dir: data_dir.into(),
            smoke_test,
        })
    }
}

/// Internal user-event payload routed through the tao event loop.
///
/// wry / tao require all window + webview API calls to happen on the
/// event-loop thread. The stdin reader thread, and the tick timer, post
/// here instead of touching wry directly.
#[derive(Debug, Clone)]
enum UserEvent {
    /// A command parsed from the parent's stdin.
    Command(HostCommand),
    /// Periodic wake-up fired by the tick thread every `TICK_PERIOD`.
    Tick,
    /// IPC from the injected JS: login state changed. Routed through the
    /// event loop so state mutation + event emission stay in one place.
    LoginChanged(LoginState),
    /// Stdin closed (parent exited or dropped us). Triggers graceful exit.
    StdinEof,
    /// The offline smoke-test page executed JavaScript and reached the host.
    SmokeTestReady,
}

fn main() -> ExitCode {
    // Must be set BEFORE WebView2 initialises (which happens when the
    // first `WebViewBuilder::build` runs). These flags pass straight
    // through to the underlying Chromium/Edge process:
    //
    // * `--mute-audio` silences the whole process at the audio-output
    //   level, regardless of what JS does. This is the only reliable
    //   way to stop stream audio leaking out of the hidden sidecar
    //   window - JS-level mutes lose races with React re-renders and
    //   don't cover Web-Workers / Web Audio routed streams.
    // * `--autoplay-policy=user-gesture-required` prevents the player
    //   from programmatically calling `.play()` at all, so the HLS
    //   pipeline doesn't even start.
    // * `--disable-features=AutoplayIgnoreWebAudio` tightens the
    //   autoplay policy to also cover `AudioContext`.
    std::env::set_var(
        "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
        "--mute-audio --autoplay-policy=user-gesture-required --disable-features=AutoplayIgnoreWebAudio",
    );

    // Tracing -> stderr. We reserve stdout for the protocol.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let Some(StartupOptions {
        data_dir,
        smoke_test,
    }) = StartupOptions::parse(std::env::args_os().skip(1))
    else {
        eprintln!("usage: crust-webview [--smoke-test] <data-directory>");
        return ExitCode::from(2);
    };
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        return startup_error("data_dir.create", e);
    }

    info!("crust-webview host starting; data_dir={data_dir:?}");
    let mut smoke_deadline = smoke_test.then(|| Instant::now() + SMOKE_TEST_TIMEOUT);

    // tao EventLoop on the main thread - that's where we are.
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Stdin reader: parse commands and forward via proxy.
    if !smoke_test {
        let proxy = proxy.clone();
        std::thread::Builder::new()
            .name("crust-webview-stdin".into())
            .spawn(move || stdin_reader(proxy))
            .expect("spawn stdin reader");
    }

    // Tick timer: fire UserEvent::Tick every TICK_PERIOD, with a fast
    // first tick so login detection + first-navigation don't have to
    // wait the full 30 s after startup.
    if !smoke_test {
        let proxy = proxy.clone();
        std::thread::Builder::new()
            .name("crust-webview-tick".into())
            .spawn(move || {
                std::thread::sleep(Duration::from_secs(3));
                if proxy.send_event(UserEvent::Tick).is_err() {
                    return;
                }
                loop {
                    std::thread::sleep(TICK_PERIOD);
                    if proxy.send_event(UserEvent::Tick).is_err() {
                        return;
                    }
                }
            })
            .expect("spawn tick timer");
    }

    // One-shot `std::sync::mpsc` shared with the IPC handler closure so it
    // can post user events even after the event loop takes ownership.
    let (ipc_event_tx, ipc_event_rx) = std_mpsc::channel::<UserEvent>();

    // Pump internal ipc_event_rx -> event loop user events on yet another
    // thread. wry's ipc_handler runs on its own thread and needs a
    // thread-safe handoff; EventLoopProxy is Send+Sync and works directly.
    // Using a channel between ipc_handler and a forwarder thread keeps the
    // hot closure tiny.
    {
        let proxy = proxy.clone();
        std::thread::Builder::new()
            .name("crust-webview-ipc-forward".into())
            .spawn(move || {
                while let Ok(evt) = ipc_event_rx.recv() {
                    if proxy.send_event(evt).is_err() {
                        return;
                    }
                }
            })
            .expect("spawn ipc forward");
    }

    let window = match WindowBuilder::new()
        .with_title("Crust - Twitch sign-in")
        .with_inner_size(LogicalSize::new(960.0, 720.0))
        .with_visible(false)
        .build(&event_loop)
    {
        Ok(w) => w,
        Err(e) => return startup_error("window.build", e),
    };

    let mut web_context = WebContext::new(Some(data_dir));
    let ipc_event_tx_for_handler = ipc_event_tx.clone();
    // Initial URL is the chat popout for Twitch's own channel. It stays
    // inside the twitch.tv cookie jar (so the login probe can read
    // `auth-token`), but has no `<video>` / `<audio>` element - so no
    // stream audio leaks out of the hidden sidecar window at startup,
    // before any `SetActiveChannel` has arrived to redirect us to the
    // user's actual focused channel.
    //
    // Additionally, `MUTE_BOOTSTRAP` is injected on every navigation as
    // defense-in-depth. If Twitch ever redirects us to a page with media
    // elements, they'll come up muted.
    let builder = WebViewBuilder::new_with_web_context(&mut web_context)
        .with_initialization_script(crust_webview::js::MUTE_BOOTSTRAP)
        .with_ipc_handler(move |req| {
            handle_ipc(req.body().as_str(), &ipc_event_tx_for_handler, smoke_test)
        });
    let builder = if smoke_test {
        builder.with_html(SMOKE_TEST_HTML)
    } else {
        builder.with_url("https://www.twitch.tv/popout/twitch/chat")
    };
    let webview = match build_webview(builder, &window) {
        Ok(wv) => wv,
        Err(e) => return startup_error("webview.build", e),
    };
    info!("crust-webview host initialized");

    let mut state = WebviewState::new();
    let mut last_login = LoginState::Unknown;
    let mut enabled = true;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = if let Some(deadline) = smoke_deadline {
            if Instant::now() >= deadline {
                warn!("crust-webview smoke test timed out waiting for JavaScript IPC");
                *control_flow = ControlFlow::ExitWithCode(1);
                return;
            }
            ControlFlow::WaitUntil(deadline)
        } else {
            ControlFlow::Wait
        };
        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                // User closed the sign-in window: hide, don't exit.
                window.set_visible(false);
            }
            Event::UserEvent(UserEvent::Command(cmd)) => {
                handle_command(
                    cmd,
                    &mut state,
                    &mut enabled,
                    &window,
                    &webview,
                    control_flow,
                );
            }
            Event::UserEvent(UserEvent::Tick) => {
                // LOGIN_PROBE runs unconditionally so the UI's signed-in
                // badge works even when auto-claim is toggled off - users
                // need to see the badge before they decide to flip the
                // toggle. CLAIM_CLICK + BALANCE_PROBE are the work that's
                // gated on the user's explicit opt-in.
                debug!(
                    "crust-webview: tick enabled={} login={:?} channel={:?}",
                    enabled,
                    state.login_state(),
                    state.active_channel()
                );
                let _ = webview.evaluate_script(crust_webview::js::LOGIN_PROBE);
                if enabled
                    && state.login_state() == LoginState::LoggedIn
                    && state.active_channel().is_some()
                {
                    let _ = webview.evaluate_script(crust_webview::js::CLAIM_CLICK);
                    let _ = webview.evaluate_script(crust_webview::js::BALANCE_PROBE);
                }
            }
            Event::UserEvent(UserEvent::LoginChanged(new_state)) => {
                let was_logged_in = state.login_state() == LoginState::LoggedIn;
                state.set_login_detected(new_state == LoginState::LoggedIn);
                window.set_visible(state.is_visible());
                // On the Unknown->LoggedIn transition, navigate to the
                // active channel if we've already been told which one.
                // The parent sends `SetActiveChannel` within seconds of
                // startup, long before the 30-s tick confirms login, so
                // without this we'd sit on the initial popout URL
                // forever - no `.community-points-summary` widget there,
                // so no claims + no balance probe matches.
                if !was_logged_in && new_state == LoginState::LoggedIn {
                    if let Some(ch) = state.active_channel() {
                        let url = crust_webview::js::channel_url(ch);
                        info!("crust-webview: login detected, navigating to {url}");
                        let _ = webview.load_url(&url);
                    }
                }
                if new_state != last_login {
                    last_login = new_state;
                    emit(&HostEvent::LoginState {
                        state: LoginStateWire::from(new_state),
                    });
                }
            }
            Event::UserEvent(UserEvent::StdinEof) => {
                info!("crust-webview: stdin closed; shutting down");
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(UserEvent::SmokeTestReady) if smoke_test => {
                smoke_deadline = None;
                info!("crust-webview smoke test passed: JavaScript IPC received");
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    })
}

fn startup_error(location: &str, error: impl std::fmt::Display) -> ExitCode {
    emit(&HostEvent::ScriptError {
        location: location.into(),
        message: error.to_string(),
    });
    emit(&HostEvent::Exited);
    ExitCode::FAILURE
}

fn build_webview<'a>(
    builder: WebViewBuilder<'a>,
    window: &'a tao::window::Window,
) -> wry::Result<wry::WebView> {
    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
    ))]
    {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;

        // Tao owns the GTK event loop and container on both X11 and Wayland.
        // Wry's raw-handle build path requires its separate X11 feature.
        let container = window
            .default_vbox()
            .ok_or_else(|| std::io::Error::other("the host window has no default GTK container"))?;
        builder.build_gtk(container)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
    )))]
    {
        builder.build(window)
    }
}

fn handle_command(
    cmd: HostCommand,
    state: &mut WebviewState,
    enabled: &mut bool,
    window: &tao::window::Window,
    webview: &wry::WebView,
    control_flow: &mut ControlFlow,
) {
    match cmd {
        HostCommand::OpenLogin => {
            state.open_login_window();
            window.set_visible(true);
            let _ = webview.load_url("https://www.twitch.tv/login");
        }
        HostCommand::SetActiveChannel { login } => {
            // Only navigate on actual change. The parent republishes the
            // active channel every ~5 s on its maintenance tick; without
            // this guard the page would reload continuously, dropping
            // cached state and (on the main channel page) restarting the
            // video player every tick.
            let prev = state.active_channel().map(str::to_owned);
            state.set_active_channel(login);
            let now = state.active_channel().map(str::to_owned);
            if prev != now && state.login_state() == LoginState::LoggedIn {
                if let Some(ch) = now.as_deref() {
                    let _ = webview.load_url(&crust_webview::js::channel_url(ch));
                }
            }
        }
        HostCommand::SetEnabled { enabled: flag } => {
            *enabled = flag;
        }
        HostCommand::Shutdown => {
            emit(&HostEvent::Exited);
            *control_flow = ControlFlow::Exit;
        }
    }
}

fn stdin_reader(proxy: tao::event_loop::EventLoopProxy<UserEvent>) {
    let stdin = std::io::stdin();
    let reader = BufReader::new(stdin.lock());
    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                debug!("stdin read error: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match decode_command(&line) {
            Ok(cmd) => {
                if proxy.send_event(UserEvent::Command(cmd)).is_err() {
                    return;
                }
            }
            Err(e) => {
                debug!("bad command line: {e}; raw: {line}");
            }
        }
    }
    let _ = proxy.send_event(UserEvent::StdinEof);
}

fn handle_ipc(body: &str, forward: &std_mpsc::Sender<UserEvent>, smoke_test: bool) {
    if smoke_test {
        if body == SMOKE_TEST_READY {
            let _ = forward.send(UserEvent::SmokeTestReady);
        }
        return;
    }
    match crust_webview::IncomingMessage::parse(body) {
        Ok(crust_webview::IncomingMessage::Login { logged_in }) => {
            let state = if logged_in {
                LoginState::LoggedIn
            } else {
                LoginState::LoggedOut
            };
            let _ = forward.send(UserEvent::LoginChanged(state));
        }
        Ok(crust_webview::IncomingMessage::Claimed) => {
            emit(&HostEvent::Claimed);
        }
        Ok(crust_webview::IncomingMessage::Balance { value }) => {
            emit(&HostEvent::Balance { value });
        }
        Ok(crust_webview::IncomingMessage::Error { location, message }) => {
            emit(&HostEvent::ScriptError { location, message });
        }
        Err(e) => {
            debug!("ipc parse failed: {e}; body: {body}");
        }
    }
}

/// Emit an event to stdout as one JSON line. Writes are line-flushed so
/// the parent's `BufReader::lines()` yields promptly.
fn emit(evt: &HostEvent) {
    let line = encode_event(evt);
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if writeln!(handle, "{line}").is_err() {
        // Parent is gone; nothing we can usefully do.
        return;
    }
    let _ = handle.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(args: &[&str]) -> Option<StartupOptions> {
        StartupOptions::parse(args.iter().map(OsString::from))
    }

    #[test]
    fn parent_arguments_keep_normal_startup() {
        assert_eq!(
            options(&["profile dir"]),
            Some(StartupOptions {
                data_dir: "profile dir".into(),
                smoke_test: false,
            })
        );
    }

    #[test]
    fn smoke_test_requires_an_explicit_profile_directory() {
        assert_eq!(
            options(&["--smoke-test", "temporary profile"]),
            Some(StartupOptions {
                data_dir: "temporary profile".into(),
                smoke_test: true,
            })
        );
        for args in [
            vec![],
            vec!["--smoke-test"],
            vec!["--smoke-test", ""],
            vec!["--smoke-test", "--unknown"],
            vec!["--unknown", "profile"],
            vec!["profile", "extra"],
            vec!["--smoke-test", "profile", "extra"],
        ] {
            assert_eq!(options(&args), None, "{args:?}");
        }
    }

    #[test]
    fn smoke_test_ipc_requires_the_local_page_reply() {
        let (sender, receiver) = std_mpsc::channel();
        handle_ipc(r#"{"kind":"login","logged_in":true}"#, &sender, true);
        handle_ipc("unexpected reply", &sender, true);
        assert!(receiver.try_recv().is_err());
        handle_ipc(SMOKE_TEST_READY, &sender, true);
        assert!(matches!(receiver.try_recv(), Ok(UserEvent::SmokeTestReady)));
    }

    #[test]
    fn normal_ipc_does_not_accept_smoke_test_success() {
        let (sender, receiver) = std_mpsc::channel();
        handle_ipc(SMOKE_TEST_READY, &sender, false);
        assert!(receiver.try_recv().is_err());
        handle_ipc(r#"{"kind":"login","logged_in":true}"#, &sender, false);
        assert!(matches!(
            receiver.try_recv(),
            Ok(UserEvent::LoginChanged(LoginState::LoggedIn))
        ));
    }
}

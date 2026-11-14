//! External-tool process spawners (Streamlink, custom player).
//!
//! Right-click a Twitch channel and choose "Open in Streamlink" or "Open in
//! player" to start a process configured by `AppSettings::external_tools`.
//!
//! # Error visibility
//!
//! Streamlink can exit if no video player is configured. The `spawn_*` helpers
//! capture both output streams and return the live [`Child`]. Callers use
//! [`finalize_exit`] to drain those streams and report a bounded tail when the
//! process fails.

use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read};
use std::process::{Child, Command, Stdio};

use tracing::warn;

#[cfg(windows)]
const DEFAULT_STREAMLINK_BINARY: &str = "streamlink.exe";
#[cfg(not(windows))]
const DEFAULT_STREAMLINK_BINARY: &str = "streamlink";

#[cfg(windows)]
const DEFAULT_MPV_BINARY: &str = "mpv.exe";
#[cfg(not(windows))]
const DEFAULT_MPV_BINARY: &str = "mpv";

/// Windows `CREATE_NO_WINDOW` creation flag.  Prevents the brief console
/// flash when spawning a console-subsystem binary from the GUI app.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const OUTPUT_TAIL_BYTES: usize = 64 * 1024;

/// Split `input` into argv tokens using shell-style quoting.
///
/// Single and double quotes group arguments, including empty arguments.
/// Backslashes escape double quotes and, outside quotes, whitespace or single
/// quotes. Other backslashes stay literal so Windows paths remain intact.
/// Before a double quote, each pair of backslashes produces one literal slash.
/// An unterminated quote includes the remaining text in the current argument.
pub fn split_shell_args(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut started = false;
    let mut iter = input.chars().peekable();

    while let Some(c) = iter.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                started = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                started = true;
            }
            '\\' if !in_single => {
                started = true;
                let mut slashes = 1;
                while iter.peek() == Some(&'\\') {
                    iter.next();
                    slashes += 1;
                }
                match iter.peek().copied() {
                    Some('"') => {
                        cur.extend(std::iter::repeat_n('\\', slashes / 2));
                        if slashes % 2 != 0 {
                            cur.push(iter.next().expect("peeked quote"));
                        }
                    }
                    Some(next) if !in_double && (next.is_whitespace() || next == '\'') => {
                        cur.extend(std::iter::repeat_n('\\', slashes - 1));
                        cur.push(iter.next().expect("peeked character"));
                    }
                    _ => cur.extend(std::iter::repeat_n('\\', slashes)),
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

fn resolve_binary(custom_path: &str, default: &str) -> String {
    let trimmed = custom_path.trim();
    if trimmed.is_empty() {
        return default.to_owned();
    }
    let p = std::path::Path::new(trimmed);
    // If the user pointed at a directory, append the expected binary name.
    if p.is_dir() {
        return p.join(default).to_string_lossy().into_owned();
    }
    trimmed.to_owned()
}

fn resolve_streamlink_binary(custom_path: &str) -> String {
    resolve_binary(custom_path, DEFAULT_STREAMLINK_BINARY)
}

fn resolve_mpv_binary(custom_path: &str) -> String {
    resolve_binary(custom_path, DEFAULT_MPV_BINARY)
}

fn default_quality(quality: &str) -> &str {
    let q = quality.trim();
    if q.is_empty() {
        "best"
    } else {
        q
    }
}

/// Apply cross-platform spawn defaults: redirect stdin to null, pipe both
/// stdout and stderr so the caller can surface exit diagnostics (Streamlink
/// / mpv split error output across both streams on Windows), and hide the
/// console window on Windows so the user doesn't see a terminal flash.
fn prepare_detached(cmd: &mut Command) {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

fn spawn_with(cmd: &mut Command, program: &str) -> Result<Child, String> {
    prepare_detached(cmd);
    match cmd.spawn() {
        Ok(child) => Ok(child),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            warn!("Spawn failed: {program} not found");
            Err(format!(
                "`{program}` not found. Install it or set the full path in Settings -> Integrations -> External Tools."
            ))
        }
        Err(e) => {
            warn!("Spawn failed for {program}: {e}");
            Err(format!("Failed to launch `{program}`: {e}"))
        }
    }
}

/// Return `true` when the argv already contains an explicit `--player` /
/// `-p` / `--player=...` so callers know not to auto-inject another.
fn has_player_flag(args: &[String]) -> bool {
    args.iter().any(|a| {
        let s = a.as_str();
        s == "--player" || s == "-p" || s.starts_with("--player=")
    })
}

/// Return `true` when the argv already carries an explicit
/// `--twitch-api-header` flag.
fn has_twitch_api_header(args: &[String]) -> bool {
    args.iter()
        .any(|a| a == "--twitch-api-header" || a.starts_with("--twitch-api-header="))
}

/// When a Twitch session token is set, prepend a
/// `--twitch-api-header "Authorization=OAuth <token>"` pair so Streamlink
/// authenticates the HLS request against Twitch's GQL playback edge
/// matching what the web player does, enabling Turbo / subscriber ad-skip,
/// and avoiding 403s on age-restricted streams.
///
/// Also forces `--twitch-purge-client-integrity`: Twitch binds the
/// client-integrity token to the session that generated it, so a cached
/// integrity token from an anonymous (or prior) session will cause
/// `Unauthorized` errors the moment we switch to an authenticated request.
/// Purging every launch is cheap (streamlink regenerates it in ~1s).
///
/// This must be the browser `auth-token` cookie value, **not** the chat-IRC
/// OAuth token: Twitch rejects the latter with `Unauthorized`.
fn maybe_inject_twitch_auth(args: &mut Vec<String>, session_token: &str) {
    if session_token.trim().is_empty() || has_twitch_api_header(args) {
        return;
    }
    // Strip a leading `oauth:` prefix if present so the outgoing header always
    // looks like `Authorization=OAuth <hex>`, regardless of whether the user
    // pasted the IRC-format or the raw cookie value.
    let bare = session_token.trim().trim_start_matches("oauth:");
    args.insert(0, "--twitch-api-header".to_owned());
    args.insert(1, format!("Authorization=OAuth {bare}"));
    if !args.iter().any(|a| a == "--twitch-purge-client-integrity") {
        args.insert(2, "--twitch-purge-client-integrity".to_owned());
    }
}

/// Does the given resolved program path look like a streamlink binary?
/// Used by `spawn_player` to decide whether to inject the Twitch auth header
/// into a user template whose first token expands to streamlink.
fn looks_like_streamlink(program: &str) -> bool {
    let filename = program.rsplit(['/', '\\']).next().unwrap_or(program);
    std::path::Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("streamlink"))
        .unwrap_or(false)
}

/// Spawn `streamlink [extra_args...] twitch.tv/<channel> <quality>`.
///
/// When `mpv_path` is non-empty and the user's extra args don't already
/// specify a player, `--player <resolved_mpv>` is automatically appended so
/// "Open in Streamlink" works without requiring a `streamlinkrc` file.  This
/// matters on Windows where Streamlink's default assumes VLC is installed.
///
/// Returns the [`Child`] on success; the caller should monitor exit status
/// and surface stderr on failure (see module docs).
pub fn spawn_streamlink(
    channel: &str,
    path: &str,
    quality: &str,
    extra_args: &str,
    mpv_path: &str,
    session_token: &str,
) -> Result<Child, String> {
    let channel = channel.trim().trim_start_matches('#');
    if channel.is_empty() {
        return Err("No channel to open".to_owned());
    }
    let binary = resolve_streamlink_binary(path);
    let mut cmd = Command::new(&binary);

    let mut user_args = split_shell_args(extra_args);
    // Auto-pass the user's Twitch session auth-token so Turbo / subscriber
    // ad-skip works and age-restricted streams play.
    maybe_inject_twitch_auth(&mut user_args, session_token);
    // Auto-configure mpv as the player when the user has supplied an mpv path
    // and hasn't already asserted a --player flag of their own.
    if !mpv_path.trim().is_empty() && !has_player_flag(&user_args) {
        let resolved = resolve_mpv_binary(mpv_path);
        user_args.push("--player".to_owned());
        user_args.push(resolved);
    }
    for arg in user_args {
        cmd.arg(arg);
    }
    let url = format!("twitch.tv/{channel}");
    cmd.arg(&url).arg(default_quality(quality));
    spawn_with(&mut cmd, &binary)
}

fn expand_player_args(
    template: &str,
    channel: &str,
    quality: &str,
    mpv: &str,
    streamlink: &str,
) -> Vec<String> {
    let url = format!("twitch.tv/{channel}");
    split_shell_args(template)
        .into_iter()
        .map(|arg| {
            let mut expanded = String::with_capacity(arg.len());
            let mut rest = arg.as_str();
            while let Some(start) = rest.find('{') {
                expanded.push_str(&rest[..start]);
                rest = &rest[start..];
                let Some(end) = rest.find('}') else {
                    break;
                };
                expanded.push_str(match &rest[1..end] {
                    "channel" => channel,
                    "url" => &url,
                    "quality" => quality,
                    "mpv" => mpv,
                    "streamlink" => streamlink,
                    _ => &rest[..=end],
                });
                rest = &rest[end + 1..];
            }
            expanded.push_str(rest);
            expanded
        })
        .collect()
}

/// Spawn the user's custom player template with `{channel}`, `{url}`,
/// `{quality}`, `{mpv}`, and `{streamlink}` substituted.
///
/// `{mpv}` / `{streamlink}` expand to the configured absolute paths, or to
/// the platform default binary name when the corresponding setting is empty.
/// Substitution happens after argument parsing, so each value stays in its
/// argument even when it contains spaces, quotes, or another placeholder.
pub fn spawn_player(
    channel: &str,
    template: &str,
    quality: &str,
    mpv_path: &str,
    streamlink_path: &str,
    session_token: &str,
) -> Result<Child, String> {
    let channel = channel.trim().trim_start_matches('#');
    if channel.is_empty() {
        return Err("No channel to open".to_owned());
    }
    let template = template.trim();
    if template.is_empty() {
        return Err(
            "Player command is empty. Set one in Settings -> Integrations -> External Tools."
                .to_owned(),
        );
    }
    let mut tokens = expand_player_args(
        template,
        channel,
        default_quality(quality),
        &resolve_mpv_binary(mpv_path),
        &resolve_streamlink_binary(streamlink_path),
    );
    if tokens.first().is_none_or(|program| program.is_empty()) {
        return Err("Player command is empty after expansion".to_owned());
    }
    let program = tokens.remove(0);
    // When the template points at streamlink as its program, inject the
    // Twitch auth header for the same reasons as the direct "Open in
    // Streamlink" path.  Otherwise the template is running a player that
    // handles auth itself (e.g. via a URL plugin), so we leave it alone.
    if looks_like_streamlink(&program) {
        maybe_inject_twitch_auth(&mut tokens, session_token);
    }
    let mut cmd = Command::new(&program);
    for arg in tokens {
        cmd.arg(arg);
    }
    spawn_with(&mut cmd, &program)
}

/// Join the last few non-empty lines of `raw` with a pipe separator, to
/// produce a compact single-line tail that fits in a chat system notice.
fn stream_tail(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let mut tail: Vec<String> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .rev()
        .take(5)
        .map(|s| s.trim().to_owned())
        .collect();
    tail.reverse();
    tail.join(" | ")
}

fn read_output_tail(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut tail = VecDeque::with_capacity(OUTPUT_TAIL_BYTES);
    let mut buffer = [0_u8; 8192];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let excess = (tail.len() + read).saturating_sub(OUTPUT_TAIL_BYTES);
        tail.drain(..excess);
        tail.extend(&buffer[..read]);
    }
    Ok(tail.into_iter().collect())
}

type OutputReader = std::thread::JoinHandle<io::Result<Vec<u8>>>;

fn finish_output_reader(reader: Option<OutputReader>) -> Result<Vec<u8>, String> {
    match reader {
        Some(reader) => reader
            .join()
            .map_err(|_| "output reader stopped unexpectedly".to_owned())?
            .map_err(|error| format!("could not read process output: {error}")),
        None => Ok(Vec::new()),
    }
}

/// Convert a [`Child`]'s exit result into a user-facing error message.
///
/// Returns `Ok(())` when the process exited successfully, `Err(msg)` when it
/// exited nonzero or the wait itself failed. Both output pipes are drained
/// concurrently, retaining at most 64 KiB each for exit diagnostics.
///
/// Designed to be called from a `spawn_blocking` task because
/// [`Child::wait`] blocks the current thread.
pub fn finalize_exit(mut child: Child, label: &str) -> Result<(), String> {
    let stdout = child
        .stdout
        .take()
        .map(|pipe| std::thread::spawn(move || read_output_tail(pipe)));
    let stderr = child
        .stderr
        .take()
        .map(|pipe| std::thread::spawn(move || read_output_tail(pipe)));
    let status = child.wait();
    let stdout = finish_output_reader(stdout);
    let stderr = finish_output_reader(stderr);
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => {
            let code = status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_owned());
            let stderr_tail = stderr.map(|bytes| stream_tail(&bytes)).unwrap_or_else(|error| error);
            let stdout_tail = stdout.map(|bytes| stream_tail(&bytes)).unwrap_or_else(|error| error);
            let combined = match (stderr_tail.is_empty(), stdout_tail.is_empty()) {
                (false, false) => format!("{stderr_tail} | {stdout_tail}"),
                (false, true) => stderr_tail,
                (true, false) => stdout_tail,
                (true, true) => String::new(),
            };
            if combined.is_empty() {
                Err(format!(
                    "{label} exited with status {code} (no output; try running the command manually to see why)"
                ))
            } else {
                Err(format!("{label} exited ({code}): {combined}"))
            }
        }
        Err(e) => Err(format!("{label} wait failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_simple() {
        assert_eq!(split_shell_args("a b c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn split_double_quoted() {
        assert_eq!(
            split_shell_args(r#"mpv --title "hello world" file"#),
            vec!["mpv", "--title", "hello world", "file"],
        );
    }

    #[test]
    fn split_single_quoted_preserves_dollars() {
        assert_eq!(
            split_shell_args(r#"echo '$HOME stays literal'"#),
            vec!["echo", "$HOME stays literal"],
        );
    }

    #[test]
    fn split_backslash_escape() {
        assert_eq!(split_shell_args(r#"a\ b c"#), vec!["a b", "c"]);
    }

    #[test]
    fn split_preserves_windows_and_unc_paths() {
        assert_eq!(
            split_shell_args(
                r#""C:\Program Files\mpv\mpv.exe" --config-dir=C:\Users\viewer\mpv "\\server\share\video file.mp4""#
            ),
            vec![
                r"C:\Program Files\mpv\mpv.exe",
                r"--config-dir=C:\Users\viewer\mpv",
                r"\\server\share\video file.mp4",
            ]
        );
        assert_eq!(split_shell_args("C:\\media\\"), vec!["C:\\media\\"]);
    }

    #[test]
    fn split_keeps_empty_quoted_arguments_and_escaped_quotes() {
        assert_eq!(
            split_shell_args(r#"mpv "" '' --title "a \"quoted\" title""#),
            vec!["mpv", "", "", "--title", "a \"quoted\" title"]
        );
    }

    #[test]
    fn split_handles_quoted_directories_with_trailing_backslashes() {
        assert_eq!(
            split_shell_args(r#"--config-dir "C:\cache\\" --other-dir 'C:\other\' --flag"#),
            vec!["--config-dir", "C:\\cache\\", "--other-dir", "C:\\other\\", "--flag"]
        );
    }

    #[test]
    fn player_template_expands_paths_as_single_arguments() {
        let mpv = r"C:\Program Files\mpv\mpv.exe";
        let streamlink = r"\\server\tools\streamlink.exe";
        assert_eq!(
            expand_player_args(
                r#"{streamlink} --player "{mpv}" {url} {quality} --title "Watching {channel}""#,
                "rustlang",
                "best",
                mpv,
                streamlink,
            ),
            vec![
                streamlink,
                "--player",
                mpv,
                "twitch.tv/rustlang",
                "best",
                "--title",
                "Watching rustlang",
            ]
        );
    }

    #[test]
    fn player_template_values_are_not_reparsed_or_expanded_again() {
        let mpv = "/players/{streamlink}/a \"quoted\" player";
        assert_eq!(
            expand_player_args(
                r#"{mpv} --quality={quality} "{channel}" {unknown} trailing{"#,
                "channel with spaces",
                "best --other-flag",
                mpv,
                "streamlink",
            ),
            vec![
                mpv,
                "--quality=best --other-flag",
                "channel with spaces",
                "{unknown}",
                "trailing{",
            ]
        );
    }

    #[test]
    fn process_output_retains_only_the_bounded_tail() {
        let mut output = vec![b'x'; OUTPUT_TAIL_BYTES * 8];
        output.extend_from_slice(b"\nlast output line\n");
        let tail = read_output_tail(io::Cursor::new(&output)).unwrap();
        assert_eq!(tail.len(), OUTPUT_TAIL_BYTES);
        assert_eq!(tail, output[output.len() - OUTPUT_TAIL_BYTES..]);
        assert!(stream_tail(&tail).ends_with("last output line"));
    }

    #[test]
    fn monitor_drains_both_output_streams_and_reports_exit() {
        const CHILD_FLAG: &str = "CRUST_EXTERNAL_OUTPUT_TEST_CHILD";
        if std::env::var_os(CHILD_FLAG).is_some() {
            use std::io::Write;
            let output = vec![b'x'; OUTPUT_TAIL_BYTES * 8];
            io::stdout().write_all(&output).unwrap();
            io::stderr().write_all(&output).unwrap();
            println!("\nstdout sentinel");
            eprintln!("\nstderr sentinel");
            std::process::exit(7);
        }
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "external::tests::monitor_drains_both_output_streams_and_reports_exit",
                "--nocapture",
            ])
            .env(CHILD_FLAG, "1");
        let child = spawn_with(&mut command, "process output fixture").unwrap();
        let error = finalize_exit(child, "fixture").unwrap_err();
        assert!(error.contains("exited (7)"), "{error}");
        assert!(error.contains("stdout sentinel"));
        assert!(error.contains("stderr sentinel"));
        assert!(error.len() < 2 * OUTPUT_TAIL_BYTES + 100);
    }

    #[test]
    fn split_empty_is_empty() {
        assert!(split_shell_args("").is_empty());
        assert!(split_shell_args("   \t  ").is_empty());
    }

    #[test]
    fn resolve_binary_empty_uses_path() {
        assert_eq!(resolve_streamlink_binary(""), DEFAULT_STREAMLINK_BINARY);
        assert_eq!(resolve_streamlink_binary("   "), DEFAULT_STREAMLINK_BINARY);
        assert_eq!(resolve_mpv_binary(""), DEFAULT_MPV_BINARY);
    }

    #[test]
    fn resolve_binary_with_full_path_is_passthrough() {
        assert_eq!(
            resolve_streamlink_binary("/usr/local/bin/streamlink"),
            "/usr/local/bin/streamlink"
        );
        assert_eq!(resolve_mpv_binary("/opt/mpv/mpv"), "/opt/mpv/mpv");
    }

    #[test]
    fn twitch_auth_injected_when_token_present() {
        let mut args: Vec<String> = vec!["--twitch-disable-ads".to_owned()];
        maybe_inject_twitch_auth(&mut args, "abc123");
        assert_eq!(
            args,
            vec![
                "--twitch-api-header",
                "Authorization=OAuth abc123",
                "--twitch-purge-client-integrity",
                "--twitch-disable-ads",
            ]
        );
    }

    #[test]
    fn twitch_auth_skips_purge_flag_when_already_present() {
        let mut args: Vec<String> = vec!["--twitch-purge-client-integrity".to_owned()];
        maybe_inject_twitch_auth(&mut args, "abc");
        let purges = args
            .iter()
            .filter(|a| a.as_str() == "--twitch-purge-client-integrity")
            .count();
        assert_eq!(purges, 1);
    }

    #[test]
    fn twitch_auth_strips_oauth_prefix_from_irc_format_token() {
        let mut args: Vec<String> = Vec::new();
        maybe_inject_twitch_auth(&mut args, "oauth:deadbeef");
        assert_eq!(args[0], "--twitch-api-header");
        assert_eq!(args[1], "Authorization=OAuth deadbeef");
        assert_eq!(args[2], "--twitch-purge-client-integrity");
    }

    #[test]
    fn twitch_auth_skipped_when_token_empty() {
        let mut args: Vec<String> = Vec::new();
        maybe_inject_twitch_auth(&mut args, "");
        assert!(args.is_empty());

        let mut args: Vec<String> = Vec::new();
        maybe_inject_twitch_auth(&mut args, "   ");
        assert!(args.is_empty());
    }

    #[test]
    fn twitch_auth_skipped_when_user_already_specified_header() {
        let mut args: Vec<String> = vec![
            "--twitch-api-header".to_owned(),
            "Authorization=OAuth existing".to_owned(),
        ];
        maybe_inject_twitch_auth(&mut args, "new");
        assert_eq!(args[1], "Authorization=OAuth existing");
        assert_eq!(args.len(), 2);
    }

    #[test]
    fn looks_like_streamlink_detects_common_names() {
        assert!(looks_like_streamlink("streamlink"));
        assert!(looks_like_streamlink("streamlink.exe"));
        assert!(looks_like_streamlink("/usr/bin/streamlink"));
        assert!(looks_like_streamlink(
            r"C:\Program Files\Streamlink\bin\streamlink.exe"
        ));
        assert!(!looks_like_streamlink("mpv"));
        assert!(!looks_like_streamlink("/usr/bin/vlc"));
    }

    #[test]
    fn has_player_flag_detects_variants() {
        let short = vec!["-p".to_owned(), "mpv".to_owned()];
        let long = vec!["--player".to_owned(), "mpv".to_owned()];
        let eq = vec!["--player=mpv".to_owned()];
        let none = vec!["--twitch-disable-ads".to_owned()];
        assert!(has_player_flag(&short));
        assert!(has_player_flag(&long));
        assert!(has_player_flag(&eq));
        assert!(!has_player_flag(&none));
    }
}

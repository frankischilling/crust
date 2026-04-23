//! Crash handler + crash report reader.
//!
//! Installs a [`std::panic::set_hook`]-based handler as early as
//! possible in startup so panics on any thread (main UI, tokio
//! worker, reducer loop, etc.) are flushed to disk before the
//! process tears down.  On the next launch [`load_existing_reports`]
//! scans the crash directory and hands the result to `CrustApp` via
//! [`crust_ui::CrustApp::set_pending_crash_reports`], where the crash
//! viewer offers "view / copy / delete / dismiss all / restart" etc.
//!
//! ## What ends up in a report
//!
//! * header: app version + build profile, UTC + local timestamps,
//!   OS / arch / target triple, host CPU count and the effective
//!   display server (Wayland / X11 / none).
//! * run-identity: short `run_id` so sentinels and reports can be
//!   correlated.
//! * thread/location/payload of the panic.
//! * force-captured `std::backtrace::Backtrace` (ignores
//!   `RUST_BACKTRACE`).
//! * the last ~512 tracing events (INFO and above by default) via a
//!   lock-free in-memory ring buffer pushed from a custom
//!   `tracing_subscriber::Layer`.
//! * a snapshot of [`crust_storage::AppSettings`] (no secrets - tokens
//!   are kept in the OS keyring and `AppSettings` stores only a
//!   fallback blob that we also scrub before serializing).
//!
//! ## Abnormal shutdown detection
//!
//! Right after the panic hook is installed we drop a small sentinel
//! file at `{crash_dir}/sessions/<run_id>.session` describing the
//! current process (pid, start time, version, command line).  It is
//! deleted on clean shutdown by [`clear_session_sentinel`].  If any
//! sentinels are found on startup whose `run_id` doesn't already have
//! a matching `crash-*.txt` report, we surface them to the UI as
//! "session ended abnormally" reports so killed/SEGV'd runs aren't
//! lost.
//!
//! This mirrors the intent of Chatterino's
//! `src/singletons/CrashHandler.hpp`, minus the Crashpad IPC: Crust
//! runs inside a single-process egui app so an in-process panic hook
//! plus a session sentinel is enough.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crust_ui::CrashReportMeta;

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const REPORT_EXT: &str = "txt";
const SESSION_EXT: &str = "session";
/// Cap on retained reports (newest first).  Surplus is deleted on
/// startup so repeated crashes don't balloon disk usage.
const MAX_RETAINED_REPORTS: usize = 20;
/// Cap on retained session sentinels.  Same idea, but surplus mostly
/// means the user killed the app many times without it ever
/// panicking - rare but worth bounding.
const MAX_RETAINED_SESSIONS: usize = 40;
/// Upper bound on the tracing log ring attached to each report.
const LOG_RING_CAP: usize = 512;
/// Upper bound on a report's read-back size for UI preview.  Real
/// reports are typically <32 KiB; we cap preview at 64 KiB so a
/// runaway log-tail doesn't bloat in-memory state.
const PREVIEW_MAX_BYTES: usize = 64 * 1024;

/// Identifier for the *current* run.  Stable for the lifetime of the
/// process; embedded in the session sentinel and in every crash
/// report written by this process.
static RUN_ID: OnceLock<String> = OnceLock::new();
/// Absolute path of the session sentinel for the current run.
static SESSION_SENTINEL_PATH: OnceLock<PathBuf> = OnceLock::new();
/// Last-known settings snapshot, refreshed by
/// [`update_settings_snapshot`] and by the storage-crate persist
/// hook.  Read with `try_lock` inside the panic hook so a prior
/// poisoning never blocks report writing.
static SETTINGS_SNAPSHOT: OnceLock<Mutex<String>> = OnceLock::new();
/// Rolling tail of tracing events.  Populated by [`CrashLogLayer`].
static LOG_RING: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
/// Guard against nested/re-entrant panic writes.  Prevents a panic
/// inside the hook (for instance, a tracing Visit that panics) from
/// aborting the process with a double-panic before the original
/// report is flushed.
static PANIC_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
/// Counter used to disambiguate multiple crash reports produced in
/// the same wall-clock second (e.g. two background workers panicking
/// simultaneously).
static CRASH_SEQ: AtomicU64 = AtomicU64::new(0);

// Public API

/// Return the crash directory under the platform-specific data dir.
/// Returns `None` when no project-dirs path is resolvable (headless
/// test environments, unusual user contexts, etc.).
pub fn crashes_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "crust", "crust")
        .map(|dirs| dirs.data_dir().join("logs").join("crashes"))
}

/// Build a tracing [`tracing_subscriber::Layer`] that feeds the crash
/// log ring.  Layer the return value into your subscriber right next
/// to the fmt + env-filter layers so every logged event ends up in
/// the ring as well as on stderr.
pub fn tracing_layer() -> CrashLogLayer {
    CrashLogLayer
}

/// Install the global panic hook and session sentinel.  Safe to call
/// exactly once per process.  Subsequent calls are no-ops so the
/// caller may invoke this defensively.
pub fn install(crash_dir: PathBuf) {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    let _ = fs::create_dir_all(&crash_dir);
    let sessions_dir = crash_dir.join("sessions");
    let _ = fs::create_dir_all(&sessions_dir);

    // Generate and freeze a run id now so report + sentinel filenames
    // always agree.  Format: YYYYMMDD-HHMMSS-<pid> - human-readable,
    // sort-stable, collision-resistant for anything short of a
    // pid-reuse inside the same second.
    let now = chrono::Utc::now();
    let run_id = format!(
        "{}-pid{}",
        now.format("%Y%m%d-%H%M%S"),
        std::process::id()
    );
    let _ = RUN_ID.set(run_id.clone());

    // Write the session sentinel so we can detect abnormal shutdowns.
    // Failing here is non-fatal; we still install the panic hook.
    let sentinel_path = sessions_dir.join(format!("{run_id}.{SESSION_EXT}"));
    match write_session_sentinel(&sentinel_path, &run_id, now) {
        Ok(()) => {
            let _ = SESSION_SENTINEL_PATH.set(sentinel_path);
        }
        Err(e) => {
            eprintln!(
                "[crust] failed to write session sentinel {}: {e}",
                sentinel_path.display()
            );
        }
    }

    let prev = std::panic::take_hook();
    let crash_dir_for_hook = crash_dir.clone();
    std::panic::set_hook(Box::new(move |info| {
        // Re-entrancy guard: if a tracing/write inside the hook panics
        // we'd otherwise double-panic and abort before the first
        // report is flushed.  First write wins; subsequent panics
        // short-circuit straight to the default-stderr path.
        if PANIC_IN_FLIGHT.swap(true, Ordering::SeqCst) {
            prev(info);
            return;
        }

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            write_report(&crash_dir_for_hook, info)
        }));

        match result {
            Ok(Ok(path)) => {
                eprintln!("[crust] crash report written to {}", path.display());
            }
            Ok(Err(e)) => {
                eprintln!("[crust] failed to write crash report: {e}");
            }
            Err(_) => {
                eprintln!("[crust] crash handler itself panicked; skipping report");
            }
        }

        // Keep the sentinel on disk so the next launch surfaces it
        // even if the panic hook ran successfully - the user can see
        // "this session crashed" immediately and correlate with the
        // written report via run_id.  The normal reports-listing
        // step dedupes by run_id.

        // Reset the guard AFTER the sequence completes so the legacy
        // chain still runs.  If another thread is racing to panic
        // concurrently, it will observe `true` and take the fast
        // path above, preserving ordering.
        PANIC_IN_FLIGHT.store(false, Ordering::SeqCst);

        // Chain to the previous hook (usually the default, which
        // prints the panic to stderr).  Keeping the chain intact
        // preserves RUST_BACKTRACE output in terminals / systemd
        // journals as a secondary record.
        prev(info);
    }));
}

/// Called by the app's clean-shutdown path.  Removes the current
/// session's sentinel so the next launch does not treat it as an
/// abnormal shutdown.  Safe no-op if the sentinel path was never
/// registered (e.g. [`install`] was never called, or the sentinel
/// failed to write).
pub fn clear_session_sentinel() {
    if let Some(path) = SESSION_SENTINEL_PATH.get() {
        let _ = fs::remove_file(path);
    }
}

/// Install / replace the remembered settings snapshot.  Call once
/// at startup with the initial settings and wire
/// [`crust_storage::set_persist_hook`] to call this on every
/// subsequent save so the next crash report captures the current
/// settings, not a stale startup copy.
pub fn update_settings_snapshot(text: impl Into<String>) {
    let m = SETTINGS_SNAPSHOT.get_or_init(|| Mutex::new(String::new()));
    if let Ok(mut g) = m.lock() {
        *g = text.into();
    }
}

/// Scan `dir` for previously-written crash reports and orphan session
/// sentinels, then return them newest-first.  Sentinels with a
/// matching `crash-<run_id>-*.txt` report are suppressed (the panic
/// report supersedes them); sentinels without a matching report
/// surface as synthetic "abnormal shutdown" entries.
pub fn load_existing_reports(dir: &Path) -> Vec<CrashReportMeta> {
    let mut reports = read_crash_reports(dir);

    // Build the set of run_ids covered by a real panic report, so we
    // can suppress the synthetic sentinel entry for those.
    let mut covered: HashSet<String> = HashSet::new();
    for r in &reports {
        for line in r.preview.lines().take(16) {
            if let Some(rest) = line.strip_prefix("run_id:") {
                covered.insert(rest.trim().to_string());
                break;
            }
        }
    }

    let mut orphans = read_orphan_sentinels(dir, &covered);
    reports.append(&mut orphans);

    // Newest first via embedded timestamp (the header line), with
    // filename as a stable tiebreaker when timestamps collide (e.g.
    // two reports in the same wall-clock second or when mtime
    // resolution is coarse).
    reports.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(b.file_name.cmp(&a.file_name)));

    // Cap retained reports.  Drain the surplus and delete the
    // backing files so disk usage stays bounded.
    if reports.len() > MAX_RETAINED_REPORTS {
        for surplus in reports.drain(MAX_RETAINED_REPORTS..) {
            let _ = fs::remove_file(&surplus.path);
        }
    }

    // Cap retained sentinels in the sessions directory too, even
    // those that didn't surface as orphans (e.g. they had a matching
    // report so were dedup'd above).
    prune_old_sentinels(dir, MAX_RETAINED_SESSIONS);

    reports
}

// Tracing log ring

/// Tracing [`Layer`] that pushes every event into a bounded in-memory
/// ring.  The ring is attached to crash reports at panic time.
pub struct CrashLogLayer;

impl<S> tracing_subscriber::Layer<S> for CrashLogLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let meta = event.metadata();
        let mut body = String::new();
        let mut visitor = LogRecordVisitor(&mut body);
        event.record(&mut visitor);
        let body = body.trim();
        let ts = chrono::Utc::now().format("%H:%M:%S%.3f");
        let line = format!(
            "[{ts} {level:5} {target}] {body}",
            level = meta.level().as_str(),
            target = meta.target(),
        );
        push_log_line(line);
    }
}

struct LogRecordVisitor<'a>(&'a mut String);

impl tracing::field::Visit for LogRecordVisitor<'_> {
    fn record_debug(
        &mut self,
        field: &tracing::field::Field,
        value: &dyn std::fmt::Debug,
    ) {
        use std::fmt::Write as _;
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        use std::fmt::Write as _;
        if field.name() == "message" {
            self.0.push_str(value);
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}

fn push_log_line(line: String) {
    let ring = LOG_RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(LOG_RING_CAP)));
    if let Ok(mut g) = ring.lock() {
        while g.len() >= LOG_RING_CAP {
            g.pop_front();
        }
        g.push_back(line);
    }
}

fn snapshot_log_lines() -> Vec<String> {
    LOG_RING
        .get()
        .and_then(|m| m.try_lock().ok().map(|g| g.iter().cloned().collect()))
        .unwrap_or_default()
}

// Internal: report writing

fn payload_str(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".to_string()
}

fn current_settings_snapshot() -> String {
    SETTINGS_SNAPSHOT
        .get()
        .and_then(|m| m.try_lock().ok().map(|g| g.clone()))
        .unwrap_or_default()
}

fn write_report(
    dir: &Path,
    info: &std::panic::PanicHookInfo<'_>,
) -> std::io::Result<PathBuf> {
    fs::create_dir_all(dir)?;

    let now = chrono::Utc::now();
    let ts_file = now.format("%Y%m%d-%H%M%S").to_string();
    let ts_iso = now.to_rfc3339();
    let local_iso = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z").to_string();
    let seq = CRASH_SEQ.fetch_add(1, Ordering::SeqCst);

    let run_id_default = format!("unregistered-pid{}", std::process::id());
    let run_id = RUN_ID.get().cloned().unwrap_or(run_id_default);

    // Filename encodes time + seq so parallel panics in the same
    // second don't collide even if two workers die in lockstep.
    let path = dir.join(format!(
        "crash-{ts_file}-{seq:03}-{run_id}.{REPORT_EXT}"
    ));

    let thread = std::thread::current();
    let thread_name = thread.name().unwrap_or("<unnamed>");
    let payload = payload_str(info);
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "<unknown>".into());

    // `force_capture` honours neither `RUST_BACKTRACE` nor
    // `RUST_LIB_BACKTRACE` - we unconditionally want a trace in the
    // crash report regardless of the user's shell environment.
    let backtrace = std::backtrace::Backtrace::force_capture();

    let settings_blob = current_settings_snapshot();
    let settings_block = if settings_blob.is_empty() {
        "<not captured>".to_string()
    } else {
        settings_blob
    };

    let log_lines = snapshot_log_lines();

    let mut file = fs::File::create(&path)?;
    writeln!(file, "== Crust crash report ==")?;
    writeln!(file, "version: {APP_VERSION}")?;
    writeln!(file, "build_profile: {}", build_profile())?;
    writeln!(file, "target: {}", target_triple())?;
    writeln!(file, "run_id: {run_id}")?;
    writeln!(file, "timestamp_utc: {ts_iso}")?;
    writeln!(file, "timestamp_local: {local_iso}")?;
    writeln!(
        file,
        "os: {} / arch: {} / family: {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::consts::FAMILY,
    )?;
    writeln!(file, "cpu_cores: {}", available_parallelism_str())?;
    writeln!(file, "display_server: {}", detect_display_server())?;
    writeln!(file, "pid: {}", std::process::id())?;
    writeln!(file, "thread: {thread_name}")?;
    writeln!(file, "location: {location}")?;
    writeln!(file, "panic: {}", sanitize_single_line(&payload))?;
    writeln!(file)?;
    writeln!(file, "---- backtrace ----")?;
    writeln!(file, "{backtrace}")?;
    writeln!(file)?;
    writeln!(file, "---- recent tracing events ({}) ----", log_lines.len())?;
    if log_lines.is_empty() {
        writeln!(file, "<log ring empty>")?;
    } else {
        for line in &log_lines {
            writeln!(file, "{line}")?;
        }
    }
    writeln!(file)?;
    writeln!(file, "---- settings snapshot ----")?;
    writeln!(file, "{settings_block}")?;
    file.flush()?;

    Ok(path)
}

fn write_session_sentinel(
    path: &Path,
    run_id: &str,
    started: chrono::DateTime<chrono::Utc>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".into());
    let cmdline = std::env::args().collect::<Vec<_>>().join(" ");

    let mut file = fs::File::create(path)?;
    writeln!(file, "run_id: {run_id}")?;
    writeln!(file, "version: {APP_VERSION}")?;
    writeln!(file, "build_profile: {}", build_profile())?;
    writeln!(file, "target: {}", target_triple())?;
    writeln!(file, "pid: {}", std::process::id())?;
    writeln!(file, "started_utc: {}", started.to_rfc3339())?;
    writeln!(file, "os: {} / arch: {}", std::env::consts::OS, std::env::consts::ARCH)?;
    writeln!(file, "exe: {exe}")?;
    writeln!(file, "cmdline: {cmdline}")?;
    file.flush()?;
    Ok(())
}

// Internal: report loading

fn read_crash_reports(dir: &Path) -> Vec<CrashReportMeta> {
    let mut out = Vec::new();
    let read_dir = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return out,
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(REPORT_EXT) {
            continue;
        }
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) if n.starts_with("crash-") => n.to_string(),
            _ => continue,
        };

        let content = read_capped(&path, PREVIEW_MAX_BYTES).unwrap_or_default();
        let (timestamp, summary) = parse_report_header(&content, &file_name);

        out.push(CrashReportMeta {
            path,
            file_name,
            timestamp,
            summary,
            preview: content,
        });
    }

    out
}

fn read_orphan_sentinels(dir: &Path, covered: &HashSet<String>) -> Vec<CrashReportMeta> {
    let mut out = Vec::new();
    let sessions_dir = dir.join("sessions");
    let read_dir = match fs::read_dir(&sessions_dir) {
        Ok(rd) => rd,
        Err(_) => return out,
    };

    // Current-run sentinel is owned by this process and must not be
    // surfaced as orphaned; filter it out explicitly.
    let current_sentinel = SESSION_SENTINEL_PATH.get();

    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(SESSION_EXT) {
            continue;
        }
        if Some(&path) == current_sentinel {
            continue;
        }

        let content = read_capped(&path, PREVIEW_MAX_BYTES).unwrap_or_default();

        let mut run_id = String::new();
        let mut started = String::new();
        let mut version = String::new();
        let mut pid = String::new();
        for line in content.lines().take(12) {
            if let Some(rest) = line.strip_prefix("run_id:") {
                run_id = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("started_utc:") {
                started = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("version:") {
                version = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("pid:") {
                pid = rest.trim().to_string();
            }
        }

        if !run_id.is_empty() && covered.contains(&run_id) {
            // A real panic report already covers this session; treat
            // the sentinel as spent and remove it so it doesn't keep
            // showing up on future launches.
            let _ = fs::remove_file(&path);
            continue;
        }

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        let summary = format!(
            "Previous session ({}) ended without clean shutdown",
            if !version.is_empty() { version.as_str() } else { "unknown version" }
        );
        let timestamp = if !started.is_empty() { started } else { file_name.clone() };
        let preview = build_abnormal_shutdown_preview(&run_id, &timestamp, &version, &pid, &content);

        out.push(CrashReportMeta {
            path,
            file_name,
            timestamp,
            summary,
            preview,
        });
    }

    out
}

fn build_abnormal_shutdown_preview(
    run_id: &str,
    started: &str,
    version: &str,
    pid: &str,
    raw_sentinel: &str,
) -> String {
    let log_lines = snapshot_log_lines();
    // For abnormal shutdown from a previous process, the in-memory
    // ring is empty unless the process happened to crash while we
    // were still running (rare but possible when the current
    // session's launch overlapped).  Still worth including when
    // present.
    let mut out = String::new();
    out.push_str("== Abnormal shutdown (no panic report captured) ==\n");
    out.push_str(&format!("run_id: {run_id}\n"));
    out.push_str(&format!("started_utc: {started}\n"));
    if !version.is_empty() {
        out.push_str(&format!("version: {version}\n"));
    }
    if !pid.is_empty() {
        out.push_str(&format!("pid: {pid}\n"));
    }
    out.push('\n');
    out.push_str(
        "The previous run did not remove its session sentinel, which usually means\n\
         the process was killed (SIGKILL / power loss / OS-level exception that\n\
         terminated the process before the Rust panic hook could run). No panic\n\
         backtrace is available for this session.\n",
    );
    out.push('\n');
    out.push_str("---- session sentinel contents ----\n");
    out.push_str(raw_sentinel);
    if !raw_sentinel.ends_with('\n') {
        out.push('\n');
    }
    if !log_lines.is_empty() {
        out.push('\n');
        out.push_str(&format!(
            "---- recent tracing events from current session ({}) ----\n",
            log_lines.len()
        ));
        for l in &log_lines {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

fn prune_old_sentinels(dir: &Path, max: usize) {
    let sessions_dir = dir.join("sessions");
    let rd = match fs::read_dir(&sessions_dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };

    let mut entries: Vec<(PathBuf, Option<std::time::SystemTime>)> = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(SESSION_EXT) {
            continue;
        }
        let mtime = fs::metadata(&path).ok().and_then(|m| m.modified().ok());
        entries.push((path, mtime));
    }
    entries.sort_by(|a, b| match (b.1, a.1) {
        (Some(bb), Some(aa)) => bb.cmp(&aa),
        _ => std::cmp::Ordering::Equal,
    });
    for (p, _) in entries.into_iter().skip(max) {
        let _ = fs::remove_file(p);
    }
}

fn read_capped(path: &Path, max_bytes: usize) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = fs::File::open(path)?;
    let mut buf = Vec::with_capacity(max_bytes.min(8 * 1024));
    let mut taken = (&mut f).take(max_bytes as u64 + 1);
    taken.read_to_end(&mut buf)?;
    if buf.len() > max_bytes {
        buf.truncate(max_bytes);
        buf.extend_from_slice(b"\n\n[... truncated at 64 KiB ...]\n");
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn parse_report_header(content: &str, file_name: &str) -> (String, String) {
    let mut ts = String::new();
    let mut summary = String::new();
    for line in content.lines().take(24) {
        if let Some(rest) = line.strip_prefix("timestamp_utc:") {
            ts = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("panic:") {
            summary = rest.trim().to_string();
        }
    }
    if ts.is_empty() {
        ts = file_name
            .trim_start_matches("crash-")
            .trim_end_matches(".txt")
            .to_string();
    }
    if summary.is_empty() {
        summary = "<no panic message captured>".to_string();
    }
    (ts, summary)
}

fn sanitize_single_line(s: &str) -> String {
    // Keep the `panic:` header on a single line for easy grep-ability;
    // embedded newlines get replaced with ` | ` markers.  The full
    // multi-line text lives in the backtrace block below.
    s.replace('\r', "").replace('\n', " | ")
}

fn build_profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

fn target_triple() -> String {
    // There's no first-class std constant for the target triple, and
    // `CARGO_CFG_TARGET_*` env vars are only set for build scripts.
    // Compose from runtime `consts`; this is good enough for triage
    // even if it doesn't exactly match the rustc target string.
    format!(
        "{}-{}-{}",
        std::env::consts::ARCH,
        std::env::consts::FAMILY,
        std::env::consts::OS,
    )
}

fn available_parallelism_str() -> String {
    std::thread::available_parallelism()
        .map(|n| n.to_string())
        .unwrap_or_else(|_| "<unknown>".into())
}

fn detect_display_server() -> &'static str {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        "wayland"
    } else if std::env::var_os("DISPLAY").is_some() {
        "x11"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "unknown"
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_header_reads_timestamp_and_summary() {
        let sample = "== Crust crash report ==\nversion: 9.9.9\n\
                      run_id: 20260422-233400-pid42\n\
                      timestamp_utc: 2026-04-22T23:34:00+00:00\n\
                      os: linux / arch: x86_64\nthread: tokio-runtime-worker\n\
                      location: src/foo.rs:10:5\npanic: explicit panic here\n\n\
                      ---- backtrace ----\nstack frames …";
        let (ts, summary) = parse_report_header(sample, "crash-20260422-233400-000-run42.txt");
        assert_eq!(ts, "2026-04-22T23:34:00+00:00");
        assert_eq!(summary, "explicit panic here");
    }

    #[test]
    fn parse_header_falls_back_to_filename() {
        let (ts, summary) = parse_report_header("empty", "crash-20260422-231105.txt");
        assert_eq!(ts, "20260422-231105");
        assert_eq!(summary, "<no panic message captured>");
    }

    #[test]
    fn settings_snapshot_roundtrips() {
        update_settings_snapshot("hello = 1\n");
        assert!(current_settings_snapshot().contains("hello = 1"));
        update_settings_snapshot("hello = 2\n");
        assert!(current_settings_snapshot().contains("hello = 2"));
    }

    #[test]
    fn log_ring_retains_last_n_events() {
        // Drain first so parallel tests don't interfere.
        if let Some(m) = LOG_RING.get() {
            if let Ok(mut g) = m.lock() {
                g.clear();
            }
        }
        for i in 0..(LOG_RING_CAP + 10) {
            push_log_line(format!("line {i}"));
        }
        let snap = snapshot_log_lines();
        assert_eq!(snap.len(), LOG_RING_CAP);
        assert!(snap.first().unwrap().ends_with("10"));
        assert!(snap.last().unwrap().ends_with(&format!("{}", LOG_RING_CAP + 9)));
    }

    #[test]
    fn sanitize_single_line_flattens() {
        assert_eq!(sanitize_single_line("a\nb\r\nc"), "a | b | c");
    }

    #[test]
    fn load_existing_sorts_newest_first_and_dedupes_sentinel() {
        let tmp = std::env::temp_dir().join(format!(
            "crust_crash_test_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        ));
        let sessions = tmp.join("sessions");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&sessions).unwrap();

        // Real crash report covering run_id "A".
        fs::write(
            tmp.join("crash-20260401-120000-000-A.txt"),
            "run_id: A\ntimestamp_utc: 2026-04-01T12:00:00+00:00\npanic: boom\n",
        )
        .unwrap();
        // Orphan sentinel for run_id "B" (no matching crash report).
        fs::write(
            sessions.join("B.session"),
            "run_id: B\nversion: 9.9.9\nstarted_utc: 2026-04-02T13:00:00+00:00\npid: 999\n",
        )
        .unwrap();
        // Dedup'd sentinel for run_id "A" (matching report exists).
        fs::write(
            sessions.join("A.session"),
            "run_id: A\nversion: 9.9.9\nstarted_utc: 2026-04-01T11:59:59+00:00\npid: 111\n",
        )
        .unwrap();

        let reports = load_existing_reports(&tmp);
        // Expect two surfaced entries: the crash and the orphan sentinel.
        assert_eq!(reports.len(), 2);
        // Newest-first by timestamp: B (Apr 2) before A (Apr 1).
        assert!(reports[0].summary.contains("Previous session"));
        assert_eq!(reports[1].summary, "boom");
        // The dedup'd A.session must have been removed from disk.
        assert!(!sessions.join("A.session").exists());
        // The orphan sentinel survived because it's still "pending".
        assert!(sessions.join("B.session").exists());

        let _ = fs::remove_dir_all(&tmp);
    }
}

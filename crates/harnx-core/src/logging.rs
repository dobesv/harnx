//! The one logger every binary in this workspace installs.
//!
//! # Where logs go
//!
//! A binary declares what it *is* — [`LogSink::File`] for the ones that own the
//! terminal, [`LogSink::Stderr`] for servers and subprocesses — and this module
//! decides where the records land:
//!
//! - Terminal-UI binaries write to a file, so log lines never overwrite the TUI
//!   or corrupt piped stdout. Default `<state dir>/harnx.log`, overridable with
//!   `HARNX_LOG_PATH`.
//! - Everything else writes to stderr and lets its parent decide where that
//!   goes. `HARNX_LOG_PATH` is ignored: one process per tree owns the file, so
//!   there is exactly one writer opening it.
//!
//! [`child_output_sink`] closes the loop. A process logging to a file hands its
//! children that same file; a process logging to stderr lets them inherit. So a
//! `harnx` → `harnx-worker` → tool-server tree explains itself in one file
//! without any of the children knowing the path.
//!
//! # Configuration
//!
//! | Variable | Values | Default |
//! |---|---|---|
//! | `HARNX_LOG_LEVEL` | `off` `error` `warn` `info` `debug` `trace` | `info` |
//! | `HARNX_LOG_FORMAT` | `text` `json` | `text` |
//! | `HARNX_LOG_FILTER` | target prefix | `harnx` |
//! | `HARNX_LOG_PATH` | file path | `<state dir>/harnx.log` |
//! | `HARNX_LOG_MAX_BYTES` | `0` or u64 | `33554432` (32 MiB) |
//!
//! All five are plain env vars inherited by child processes.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use log::{LevelFilter, Log, Metadata, Record};

use crate::config_paths::{get_env_name, state_path};

/// Log file name under the state directory.
const LOG_FILE_NAME: &str = "harnx.log";

/// Default max log file size before rotation (32 MiB).
const DEFAULT_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Default target prefix. Matches every `harnx_*` crate target, since Rust
/// turns the crate name `harnx-runtime` into the target `harnx_runtime::…`.
const DEFAULT_FILTER: &str = "harnx";

/// What a binary is, which decides where its records go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSink {
    /// Binaries that draw on the terminal or write results to stdout: the
    /// `harnx` TUI and CLI. Their logs go to a file.
    File,
    /// Servers and subprocesses. Their logs go to stderr, which whoever spawned
    /// them has already pointed somewhere useful.
    Stderr,
}

/// Wire format for one record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    Json,
}

/// Where the records for this process are written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogDest {
    File(PathBuf),
    Stderr,
}

/// Everything [`init`] needs, resolved from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogSettings {
    pub level: LevelFilter,
    pub format: LogFormat,
    pub filter: String,
    pub dest: LogDest,
    /// Maximum log file size in bytes before rotation. `None` disables rotation.
    pub max_bytes: Option<u64>,
}

impl LogSettings {
    /// Human-readable destination, for the startup banner and `.info` output.
    pub fn dest_display(&self) -> String {
        match &self.dest {
            LogDest::File(path) => path.display().to_string(),
            LogDest::Stderr => "stderr".to_string(),
        }
    }
}

/// What [`init`] resolved for this process, published so [`child_output_sink`]
/// and diagnostics don't have to re-read the environment.
static CURRENT: OnceLock<LogSettings> = OnceLock::new();

/// The settings [`init`] resolved for this process, if it has run.
pub fn current() -> Option<&'static LogSettings> {
    CURRENT.get()
}

/// Read [`LogSettings`] for this process from the environment.
pub fn settings(sink: LogSink) -> LogSettings {
    resolve(
        |name| std::env::var(name).ok(),
        sink,
        state_path(LOG_FILE_NAME),
    )
}

/// Install the process-wide logger. Idempotent: a second call, or a process that
/// installed its own logger first, is a no-op — that is the desired end state,
/// not an error worth failing startup over.
///
/// Also arms the LLM trace, which is independent of the log level (it must work
/// even at `off`, being the primary tool for checking request/response
/// correctness).
pub fn init(sink: LogSink) -> Result<LogSettings> {
    init_with(settings(sink))
}

/// [`init`] with settings the caller has adjusted — for a binary with its own
/// `-v` flag, say. Prefer [`init`] unless there is something to override.
///
/// Log rotation runs ONLY in this function, once at process start before any
/// subprocesses are spawned. The child-output path ([`child_output_sink`] →
/// [`open_append`]) only ever appends — never rotates — because renaming the log
/// out from under live writers splits output across the old and new inodes.
pub fn init_with(settings: LogSettings) -> Result<LogSettings> {
    crate::llm_trace::init_from_env();

    let _ = CURRENT.set(settings.clone());

    // R1: Rotate for File destination even when parent level is Off.
    // Children still write via CURRENT.dest, so an Off parent would otherwise
    // never bound the file.
    if let LogDest::File(path) = &settings.dest {
        if let Some(max) = settings.max_bytes {
            if settings.level == LevelFilter::Off {
                // Off case: rotate but do not open a file handle for parent
                rotate_if_needed_off(path, max);
                return Ok(settings);
            }
            // Non-Off case: rotate and open under lock
            let writer: Box<dyn Write + Send> = Box::new(rotate_and_open(path, Some(max))?);
            let logger = HarnxLogger {
                level: settings.level,
                format: settings.format,
                filter: settings.filter.clone(),
                writer: Mutex::new(writer),
            };
            // Err means another logger got there first, which is fine.
            if log::set_boxed_logger(Box::new(logger)).is_ok() {
                log::set_max_level(settings.level);
            }
            return Ok(settings);
        }
    }

    // No rotation configured or non-File dest
    if settings.level == LevelFilter::Off {
        return Ok(settings);
    }

    let writer: Box<dyn Write + Send> = match &settings.dest {
        LogDest::Stderr => Box::new(std::io::stderr()),
        LogDest::File(path) => Box::new(open_append(path)?),
    };
    let logger = HarnxLogger {
        level: settings.level,
        format: settings.format,
        filter: settings.filter.clone(),
        writer: Mutex::new(writer),
    };
    // Err means another logger got there first, which is fine.
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(settings.level);
    }
    Ok(settings)
}

/// Where a child process's stdout and stderr should go. Split out from
/// [`child_output_sink`] because `Stdio` can't be compared or inspected, so this
/// is the only part of the decision a test can assert on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChildOutput {
    /// Append to our log file.
    File(PathBuf),
    /// Let the child have our own streams.
    Inherit,
    /// Discard.
    Null,
}

/// Stdio for a child process, following the rule in the module docs: hand the
/// child our log file when we have one, otherwise let it inherit our streams.
///
/// Falls back to [`Stdio::null`] when the file can't be opened. Never `inherit`
/// in that case — the callers that log to a file are the ones drawing on the
/// terminal, and child output there corrupts the display.
///
/// This function only ever opens for append — it does NOT rotate the log.
/// Rotation is confined to [`init_with`] (once at process start, before any
/// children are spawned). Renaming here would split output across inodes.
pub fn child_output_sink() -> Stdio {
    match child_output(current().map(|settings| &settings.dest)) {
        ChildOutput::File(path) => match open_append(&path) {
            Ok(file) => Stdio::from(file),
            Err(error) => {
                log::warn!("child output not captured to {}: {error:#}", path.display());
                Stdio::null()
            }
        },
        ChildOutput::Inherit => Stdio::inherit(),
        ChildOutput::Null => Stdio::null(),
    }
}

/// A process that never called [`init`] gets [`ChildOutput::Null`], not
/// `Inherit`. In practice that means a test harness, whose stdout is a pipe: a
/// child holding an inherited pipe open outlives the test that spawned it and
/// strands the harness waiting on EOF. A process that configured no logging has
/// said nothing about wanting its children's output either.
fn child_output(dest: Option<&LogDest>) -> ChildOutput {
    match dest {
        Some(LogDest::File(path)) => ChildOutput::File(path.clone()),
        Some(LogDest::Stderr) => ChildOutput::Inherit,
        None => ChildOutput::Null,
    }
}

/// Where [`child_output_sink`] sends a child's output, phrased for a message
/// that tells the user where to go looking.
pub fn child_output_destination() -> String {
    match log_file_path() {
        Some(path) => path.display().to_string(),
        None => "this process's stderr".to_string(),
    }
}

/// The log file this process writes to, if it writes to one.
pub fn log_file_path() -> Option<&'static Path> {
    match current().map(|settings| &settings.dest) {
        Some(LogDest::File(path)) => Some(path.as_path()),
        _ => None,
    }
}

/// Open the log file for appending, creating parent directories as needed.
///
/// Append, never truncate. Several processes — the front-end plus the worker
/// subtree whose stdio it redirects here — write to one file, and `O_APPEND`
/// makes every write land at EOF instead of at a per-handle offset. Without it
/// the writers clobber each other and the kernel zero-fills the gaps, which is
/// where the giant NUL runs in #880 came from. Active sessions only append;
/// rotation happens on open at process start if the file meets or exceeds the
/// size threshold.
///
/// Read access is requested alongside append even though nothing here reads the
/// file. On Windows, `append` alone produces a `FILE_APPEND_DATA` handle, and a
/// child process that inherits it as its stdout — an MSYS shell, say — fails
/// when it probes the handle with calls that need read rights. Asking for read
/// widens the handle to behave like an ordinary one. No effect on Unix, and
/// `O_APPEND` semantics are unchanged either way.
fn open_append(path: &Path) -> Result<std::fs::File> {
    crate::path::ensure_parent_exists(path)?;
    OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open log file {}", path.display()))
}

/// Take the exclusive lock if it is free, reporting contention as `Ok(false)`.
///
/// `try_lock` returns one `Err` for two outcomes that mean opposite things to
/// a rotation attempt. Another process already holding the lock is the ordinary
/// path: that process is rotating, and this one should wait or skip. An I/O
/// error means the attempt never resolved, and treating it as contention would
/// leave the caller waiting on an owner that does not exist.
fn try_lock_exclusive(file: &std::fs::File) -> std::io::Result<bool> {
    use std::fs::TryLockError;
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

/// Build sibling path by appending suffix to the filename (never use `with_extension`).
///
/// `Path::with_extension("1")` on `harnx.log` yields `harnx.1` (replaces `.log`),
/// which is wrong. This appends to the full filename instead:
/// `harnx.log` -> `harnx.log.1`, `harnx.log` -> `harnx.log.lock`.
fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    path.with_file_name(format!(
        "{}{suffix}",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    ))
}

/// Rotate log file if size exceeds threshold.
///
/// Checks file size and renames to `.1` if `>= max_bytes`. Removes any existing
/// `.1` first. Logs warning on rename failure but never returns error.
fn rotate_if_over_threshold(path: &Path, max_bytes: u64) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.len() < max_bytes {
        return;
    }
    let rotated_path = sibling_path(path, ".1");
    let _ = std::fs::remove_file(&rotated_path);
    if let Err(e) = std::fs::rename(path, &rotated_path) {
        log::warn!("failed to rotate log {}: {e}", path.display());
    }
}

/// Acquire sidecar lock with bounded retry on contention.
///
/// On `WouldBlock`, retry with short backoff up to ~1s total.
/// Returns `Ok(Some(lock_file))` if acquired, `Ok(None)` if contended past budget,
/// or `Err(_)` if locking failed entirely.
fn acquire_lock_with_retry(lock_path: &Path) -> Result<Option<std::fs::File>> {
    use std::thread;
    use std::time::Duration;

    const RETRY_INTERVAL_MS: u64 = 20;
    const MAX_RETRIES: u32 = 50; // 20ms * 50 = 1000ms

    crate::path::ensure_parent_exists(lock_path)?;

    // Lock file: we only need to lock it, not truncate.
    // Use read+write for locking, but don't truncate - lock files persist.
    #[allow(clippy::suspicious_open_options)]
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path)?;

    for attempt in 0..=MAX_RETRIES {
        match try_lock_exclusive(&lock_file) {
            Ok(true) => return Ok(Some(lock_file)),
            Ok(false) => {
                if attempt < MAX_RETRIES {
                    thread::sleep(Duration::from_millis(RETRY_INTERVAL_MS));
                }
            }
            Err(err) => {
                return Err(err).with_context(|| format!("lock {}", lock_path.display()));
            }
        }
    }

    Ok(None)
}

/// Rotate log file if over threshold and open for appending.
///
/// Locks the sidecar `<path>.lock` across both rename and open to prevent
/// concurrent processes from splitting output.
///
/// - `max_bytes == None`: skip lock and stat, just `open_append(path)`.
/// - `max_bytes == Some(max)`:
///   - Attempt sidecar lock with bounded retry (~1s).
///   - If acquired: check file size, rotate if `>= max`, then `open_append` under lock.
///   - If contended past budget: skip rotation, just `open_append` (holder is doing it).
///   - If lock fails: best-effort unlocked rotate, then `open_append`.
///
/// Never fails startup on lock/rotation errors — logs warnings and continues.
fn rotate_and_open(path: &Path, max_bytes: Option<u64>) -> Result<std::fs::File> {
    // Rotation disabled
    let Some(max) = max_bytes else {
        return open_append(path);
    };

    let lock_path = sibling_path(path, ".lock");

    match acquire_lock_with_retry(&lock_path) {
        // Lock acquired: rotate if needed, then open
        Ok(Some(_lock)) => {
            rotate_if_over_threshold(path, max);
            open_append(path)
        }
        // Contended past budget: holder is rotating, just open
        Ok(None) => {
            log::debug!(
                "sidecar lock {} contended, skipping rotation",
                lock_path.display()
            );
            open_append(path)
        }
        // Lock error: best-effort unlocked rotate
        Err(e) => {
            log::warn!(
                "failed to acquire sidecar lock {}: {e}, rotating unlocked",
                lock_path.display()
            );
            rotate_if_over_threshold(path, max);
            open_append(path)
        }
    }
}

/// Rotate log file for LevelFilter::Off case (no file handle returned).
///
/// Similar to `rotate_and_open` but only performs rotation without opening
/// the file. Used when parent level is `Off` but rotation should still happen
/// for child processes.
fn rotate_if_needed_off(path: &Path, max_bytes: u64) {
    let lock_path = sibling_path(path, ".lock");

    match acquire_lock_with_retry(&lock_path) {
        Ok(Some(_lock)) => {
            rotate_if_over_threshold(path, max_bytes);
        }
        Ok(None) => {
            log::debug!(
                "sidecar lock {} contended, skipping rotation",
                lock_path.display()
            );
        }
        Err(e) => {
            log::warn!(
                "failed to acquire sidecar lock {}: {e}, rotating unlocked",
                lock_path.display()
            );
            rotate_if_over_threshold(path, max_bytes);
        }
    }
}

/// Resolve settings from a lookup function, so tests don't touch process env.
fn resolve(
    get: impl Fn(&str) -> Option<String>,
    sink: LogSink,
    default_path: PathBuf,
) -> LogSettings {
    LogSettings {
        level: resolve_level(&get),
        format: resolve_format(&get),
        filter: non_empty(get(&get_env_name("log_filter"))).unwrap_or_else(|| {
            // Hardcoded rather than derived from `CARGO_CRATE_NAME`: that trick
            // silently broke once the logging code moved between crates.
            DEFAULT_FILTER.to_string()
        }),
        dest: resolve_dest(&get, sink, default_path),
        max_bytes: resolve_max_bytes(&get),
    }
}

fn resolve_level(get: &impl Fn(&str) -> Option<String>) -> LevelFilter {
    [get_env_name("log_level"), "RUST_LOG".to_string()]
        .iter()
        .find_map(|name| non_empty(get(name.as_str())))
        .and_then(|value| parse_level(&value))
        .unwrap_or(LevelFilter::Info)
}

fn resolve_format(get: &impl Fn(&str) -> Option<String>) -> LogFormat {
    match non_empty(get(&get_env_name("log_format")))
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("json") => LogFormat::Json,
        _ => LogFormat::Text,
    }
}

/// Resolve max bytes for log rotation.
///
/// - Unset or empty -> `Some(DEFAULT_MAX_BYTES)` (rotation enabled with default)
/// - `"0"` -> `None` (rotation disabled)
/// - Valid u64 -> `Some(n)`
/// - Unparseable/negative/garbage -> `Some(DEFAULT_MAX_BYTES)` (safe fallback)
fn resolve_max_bytes(get: &impl Fn(&str) -> Option<String>) -> Option<u64> {
    match non_empty(get(&get_env_name("log_max_bytes"))) {
        None => Some(DEFAULT_MAX_BYTES),
        Some(value) => {
            let trimmed = value.trim();
            if trimmed == "0" {
                None
            } else {
                trimmed
                    .parse::<u64>()
                    .ok()
                    .or_else(|| {
                        trimmed
                            .parse::<i64>()
                            .ok()
                            .and_then(|n| u64::try_from(n).ok())
                    })
                    .or(Some(DEFAULT_MAX_BYTES))
            }
        }
    }
}

fn resolve_dest(
    get: &impl Fn(&str) -> Option<String>,
    sink: LogSink,
    default_path: PathBuf,
) -> LogDest {
    match sink {
        // Servers ignore `HARNX_LOG_PATH` on purpose: their parent redirects
        // their stderr into its own log, so honouring the inherited value would
        // put a second writer on the same file for no gain.
        LogSink::Stderr => LogDest::Stderr,
        LogSink::File => LogDest::File(
            non_empty(get(&get_env_name("log_path")))
                .map(PathBuf::from)
                .unwrap_or(default_path),
        ),
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

/// Accept a bare level name. `RUST_LOG` also supports per-target directives;
/// those aren't honoured here, so anything unrecognised keeps the default rather
/// than silencing the process.
fn parse_level(value: &str) -> Option<LevelFilter> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" => Some(LevelFilter::Off),
        "error" => Some(LevelFilter::Error),
        "warn" => Some(LevelFilter::Warn),
        "info" => Some(LevelFilter::Info),
        "debug" => Some(LevelFilter::Debug),
        "trace" => Some(LevelFilter::Trace),
        _ => None,
    }
}

struct HarnxLogger {
    level: LevelFilter,
    format: LogFormat,
    filter: String,
    writer: Mutex<Box<dyn Write + Send>>,
}

impl Log for HarnxLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.level && metadata.target().starts_with(&self.filter)
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = match self.format {
            LogFormat::Text => format_text(record),
            LogFormat::Json => format_json(record),
        };
        // One `write_all` per record so an `O_APPEND` file interleaves cleanly
        // at line granularity when several processes share it.
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(line.as_bytes());
            let _ = writer.flush();
        }
    }

    fn flush(&self) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.flush();
        }
    }
}

fn timestamp() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// `2026-08-16T12:34:56.789Z [INFO ] 12345 harnx_runtime::client: message`
///
/// Target and pid are on every line, not just at `debug` and below: one file now
/// holds the whole process tree, so both are needed to attribute a line.
fn format_text(record: &Record<'_>) -> String {
    let mut line = String::with_capacity(128);
    let _ = writeln!(
        line,
        "{} [{:<5}] {} {}: {}",
        timestamp(),
        record.level(),
        std::process::id(),
        record.target(),
        record.args()
    );
    line
}

/// One JSON object per line, for log shippers that want structure.
fn format_json(record: &Record<'_>) -> String {
    let value = serde_json::json!({
        "ts": timestamp(),
        "level": record.level().as_str(),
        "pid": std::process::id(),
        "target": record.target(),
        "message": record.args().to_string(),
    });
    format!("{value}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Cases are tabulated rather than written as runs of consecutive asserts:
    // it keeps each expectation labelled with the input that produced it, and
    // `cargo nextest` runs a process per test, so this crate's suite stays small
    // next to the timing-sensitive integration tests elsewhere in the workspace.

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    fn default_path() -> PathBuf {
        PathBuf::from("/state/harnx.log")
    }

    fn settings_from(pairs: &[(&str, &str)], sink: LogSink) -> LogSettings {
        resolve(env(pairs), sink, default_path())
    }

    #[test]
    fn level_comes_from_harnx_log_level_then_rust_log_then_info() {
        for (value, expected) in [
            ("debug", Some(LevelFilter::Debug)),
            (" WARN ", Some(LevelFilter::Warn)),
            ("off", Some(LevelFilter::Off)),
            // Per-target directives are RUST_LOG syntax we don't parse. They must
            // read as "unrecognised", never as "off".
            ("harnx_mcp_bridge=debug", None),
            ("", None),
        ] {
            assert_eq!(parse_level(value), expected, "parse_level({value:?})");
        }

        for (pairs, expected, why) in [
            (vec![], LevelFilter::Info, "default"),
            (
                vec![("RUST_LOG", "debug")],
                LevelFilter::Debug,
                "RUST_LOG fallback",
            ),
            (
                vec![("HARNX_LOG_LEVEL", "trace"), ("RUST_LOG", "error")],
                LevelFilter::Trace,
                "HARNX_LOG_LEVEL wins over RUST_LOG",
            ),
            (
                vec![("RUST_LOG", "harnx_mcp_bridge=debug")],
                LevelFilter::Info,
                "an unrecognised directive must not silence the process",
            ),
        ] {
            let level = settings_from(&pairs, LogSink::Stderr).level;
            assert_eq!(level, expected, "{why}: {pairs:?}");
        }
    }

    #[test]
    fn format_is_text_unless_json_is_asked_for_by_name() {
        for (pairs, expected) in [
            (vec![], LogFormat::Text),
            (vec![("HARNX_LOG_FORMAT", "JSON")], LogFormat::Json),
            (vec![("HARNX_LOG_FORMAT", "logfmt")], LogFormat::Text),
        ] {
            let format = settings_from(&pairs, LogSink::Stderr).format;
            assert_eq!(format, expected, "{pairs:?}");
        }
    }

    #[test]
    fn the_default_filter_matches_every_harnx_crate_target() {
        let filter = settings_from(&[], LogSink::Stderr).filter;
        assert_eq!(filter, "harnx");
        // Rust targets use underscores (`harnx_runtime::client`); the historic
        // `harnx::serve` filter matched none of them and dropped every line.
        for target in [
            "harnx_runtime::client",
            "harnx_runtime::bootstrap",
            "harnx_serve::server",
            "harnx_core::logging",
        ] {
            assert!(
                target.starts_with(&filter) && !target.starts_with("harnx::serve"),
                "{target} should match {filter:?} but not the historic harnx::serve"
            );
        }
    }

    #[test]
    fn log_filter_narrows_to_one_crate() {
        let settings = settings_from(&[("HARNX_LOG_FILTER", "harnx_mcp_bridge")], LogSink::Stderr);
        assert_eq!(settings.filter, "harnx_mcp_bridge");
    }

    #[test]
    fn the_sink_decides_the_destination_not_the_configured_path() {
        for (pairs, sink, expected, why) in [
            (
                vec![],
                LogSink::File,
                LogDest::File(default_path()),
                "terminal binaries default to the state-dir log file",
            ),
            (
                vec![("HARNX_LOG_PATH", "/tmp/custom.log")],
                LogSink::File,
                LogDest::File(PathBuf::from("/tmp/custom.log")),
                "HARNX_LOG_PATH overrides the default",
            ),
            (
                vec![("HARNX_LOG_PATH", "")],
                LogSink::File,
                LogDest::File(default_path()),
                "an empty override falls back to the default",
            ),
            (
                // A server inherits HARNX_LOG_PATH from the front-end that spawned
                // it and must still leave that file to its single writer.
                vec![("HARNX_LOG_PATH", "/tmp/custom.log")],
                LogSink::Stderr,
                LogDest::Stderr,
                "servers ignore an inherited HARNX_LOG_PATH",
            ),
        ] {
            assert_eq!(settings_from(&pairs, sink).dest, expected, "{why}");
        }
    }

    #[test]
    fn stderr_destinations_describe_themselves_as_stderr() {
        let settings = settings_from(&[], LogSink::Stderr);
        assert_eq!(settings.dest_display(), "stderr");
    }

    #[test]
    fn child_output_follows_our_own_destination() {
        for (dest, expected, why) in [
            (
                Some(LogDest::File(default_path())),
                ChildOutput::File(default_path()),
                "a file-logging process hands children that file",
            ),
            (
                Some(LogDest::Stderr),
                ChildOutput::Inherit,
                "a stderr-logging process lets children inherit",
            ),
            (
                // Not `Inherit`: an inherited pipe outlives the child holding it,
                // which strands a test harness waiting on EOF.
                None,
                ChildOutput::Null,
                "a process with no logger discards child output",
            ),
        ] {
            assert_eq!(child_output(dest.as_ref()), expected, "{why}");
        }
    }

    /// Render one sample record. The `format_args!` temporary can't outlive the
    /// statement that builds the `Record`, so the formatting happens inside it.
    fn record_line(format: LogFormat) -> String {
        let render = |record: &Record<'_>| match format {
            LogFormat::Text => format_text(record),
            LogFormat::Json => format_json(record),
        };
        render(
            &Record::builder()
                .args(format_args!("hello {}", 42))
                .level(log::Level::Info)
                .target("harnx_core::logging")
                .build(),
        )
    }

    #[test]
    fn text_lines_carry_the_timestamp_level_pid_target_and_message() {
        let line = record_line(LogFormat::Text);
        let pid = std::process::id().to_string();
        for expected in ["[INFO ]", &pid, "harnx_core::logging: hello 42"] {
            assert!(
                line.contains(expected),
                "{expected:?} missing from {line:?}"
            );
        }
        assert!(
            line.starts_with("20") && line.ends_with('\n'),
            "expected a timestamp first and one trailing newline: {line:?}"
        );
    }

    #[test]
    fn json_lines_parse_back_with_the_documented_keys() {
        let line = record_line(LogFormat::Json);
        assert!(line.ends_with('\n'), "{line:?}");
        let value: serde_json::Value = serde_json::from_str(line.trim()).expect("valid JSON line");
        for (key, expected) in [
            ("level", serde_json::json!("INFO")),
            ("target", serde_json::json!("harnx_core::logging")),
            ("message", serde_json::json!("hello 42")),
            ("pid", serde_json::json!(std::process::id())),
        ] {
            assert_eq!(value[key], expected, "{key}");
        }
        assert!(value["ts"].as_str().is_some_and(|ts| ts.ends_with('Z')));
    }

    #[test]
    fn records_are_gated_by_both_level_and_target() {
        let logger = HarnxLogger {
            level: LevelFilter::Info,
            format: LogFormat::Text,
            filter: "harnx".to_string(),
            writer: Mutex::new(Box::new(Vec::new())),
        };
        for (target, level, expected) in [
            ("harnx_runtime::client", log::Level::Info, true),
            ("harnx_runtime::client", log::Level::Debug, false),
            ("hyper::proto", log::Level::Error, false),
        ] {
            let metadata = Metadata::builder().target(target).level(level).build();
            assert_eq!(
                logger.enabled(&metadata),
                expected,
                "{target} at {level} should{} be logged",
                if expected { "" } else { " not" }
            );
        }
    }

    #[test]
    fn the_default_log_file_is_harnx_log_under_the_state_dir() {
        // Regression test: this path used to be derived from
        // `env!("CARGO_CRATE_NAME")`, so it silently became `harnx_runtime.log`
        // when the logging code moved between crates.
        //
        // The only test here that touches process env, so it reads the real
        // `settings` rather than the pure `resolve`.
        let overrides = [
            ("HARNX_STATE_DIR", Some("/tmp/harnx-logging-test")),
            ("XDG_STATE_HOME", None),
            ("HARNX_LOG_PATH", None),
        ];
        let priors: Vec<_> = overrides
            .iter()
            .map(|(key, value)| {
                let prior = std::env::var_os(key);
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
                (key, prior)
            })
            .collect();

        let dest = settings(LogSink::File).dest;

        for (key, prior) in priors.into_iter().rev() {
            match prior {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        assert_eq!(
            dest,
            LogDest::File(PathBuf::from("/tmp/harnx-logging-test/harnx.log"))
        );
    }

    #[test]
    fn max_bytes_defaults_to_32_mib_when_unset_or_empty() {
        // Unset
        assert_eq!(
            settings_from(&[], LogSink::Stderr).max_bytes,
            Some(DEFAULT_MAX_BYTES),
            "unset HARNX_LOG_MAX_BYTES should default to 32 MiB"
        );
        // Empty string (handled by non_empty filter)
        assert_eq!(
            settings_from(&[("HARNX_LOG_MAX_BYTES", "")], LogSink::Stderr).max_bytes,
            Some(DEFAULT_MAX_BYTES),
            "empty HARNX_LOG_MAX_BYTES should default to 32 MiB"
        );
    }

    #[test]
    fn max_bytes_zero_disables_rotation() {
        assert_eq!(
            settings_from(&[("HARNX_LOG_MAX_BYTES", "0")], LogSink::Stderr).max_bytes,
            None,
            "HARNX_LOG_MAX_BYTES=0 should disable rotation"
        );
    }

    #[test]
    fn max_bytes_accepts_valid_u64_values() {
        for (value, expected, why) in [
            ("1024", Some(1024u64), "exact value"),
            ("  2048  ", Some(2048u64), "trimmed whitespace"),
            ("1048576", Some(1024 * 1024), "1 MiB"),
            (
                &format!("{}", DEFAULT_MAX_BYTES),
                Some(DEFAULT_MAX_BYTES),
                "default value explicitly",
            ),
        ] {
            assert_eq!(
                settings_from(&[("HARNX_LOG_MAX_BYTES", value)], LogSink::Stderr).max_bytes,
                expected,
                "{why}: {value}"
            );
        }
    }

    #[test]
    fn max_bytes_falls_back_to_default_on_garbage() {
        for (value, why) in [
            ("abc", "non-numeric"),
            ("-1", "negative number"),
            ("12.5", "floating point"),
            ("999999999999999999999999999", "overflow"),
        ] {
            assert_eq!(
                settings_from(&[("HARNX_LOG_MAX_BYTES", value)], LogSink::Stderr).max_bytes,
                Some(DEFAULT_MAX_BYTES),
                "garbage should fall back to default: {why}"
            );
        }
    }

    #[test]
    fn a_free_lock_is_acquired_and_a_held_one_reports_contention() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("lock.test");
        let held = std::fs::File::create(&path).expect("create lock file");
        assert!(
            try_lock_exclusive(&held).expect("first attempt"),
            "free lock should be acquired"
        );

        // A second handle to the same file stands in for the second process.
        let contender = std::fs::File::open(&path).expect("reopen lock file");
        assert!(
            !try_lock_exclusive(&contender).expect("contended attempt"),
            "held lock should report contention"
        );

        held.unlock().expect("release lock");
        assert!(
            try_lock_exclusive(&contender).expect("attempt after release"),
            "released lock should be acquired"
        );
    }

    // =====================
    // Rotation tests
    // =====================

    #[test]
    fn sibling_path_appends_suffix_to_filename() {
        // Standard .log extension
        let path = PathBuf::from("/var/log/harnx.log");
        assert_eq!(
            sibling_path(&path, ".1"),
            PathBuf::from("/var/log/harnx.log.1")
        );
        assert_eq!(
            sibling_path(&path, ".lock"),
            PathBuf::from("/var/log/harnx.log.lock")
        );

        // Custom extension (not .log)
        let custom = PathBuf::from("/tmp/app.output");
        assert_eq!(
            sibling_path(&custom, ".1"),
            PathBuf::from("/tmp/app.output.1")
        );
        assert_eq!(
            sibling_path(&custom, ".lock"),
            PathBuf::from("/tmp/app.output.lock")
        );

        // No extension
        let no_ext = PathBuf::from("/var/log/harnx");
        assert_eq!(
            sibling_path(&no_ext, ".1"),
            PathBuf::from("/var/log/harnx.1")
        );
    }

    #[test]
    fn rotate_and_open_over_threshold_rotates_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("harnx.log");
        let content = b"old log content that exceeds threshold";

        // Write content larger than threshold
        std::fs::write(&path, content).expect("write initial log");

        // Call rotate_and_open with threshold smaller than file
        let max_bytes = (content.len() - 1) as u64;
        let _file = rotate_and_open(&path, Some(max_bytes)).expect("rotate and open");

        // Assert .1 exists with old content
        let rotated = sibling_path(&path, ".1");
        assert!(rotated.exists(), "rotated file should exist");
        assert_eq!(
            std::fs::read(&rotated).expect("read rotated"),
            content,
            "rotated file should contain old content"
        );

        // Original path should be recreated (empty or fresh)
        assert!(path.exists(), "log path should be recreated");

        // Lock file should exist but we don't delete it
        let lock_path = sibling_path(&path, ".lock");
        assert!(lock_path.exists(), "lock file should exist");
    }

    #[test]
    fn rotate_and_open_under_threshold_leaves_file_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("harnx.log");
        let content = b"small content";

        std::fs::write(&path, content).expect("write initial log");

        // Threshold larger than file
        let max_bytes = (content.len() + 1000) as u64;
        let _file = rotate_and_open(&path, Some(max_bytes)).expect("rotate and open");

        // No .1 should be created
        let rotated = sibling_path(&path, ".1");
        assert!(!rotated.exists(), "rotated file should not exist");

        // Original unchanged
        assert_eq!(
            std::fs::read(&path).expect("read log"),
            content,
            "original content should be unchanged"
        );
    }

    #[test]
    fn rotate_and_open_overwrites_existing_rotated_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("harnx.log");
        let old_content = b"old log content exceeding threshold";
        let sentinel = b"sentinel from previous rotation";

        // Create existing .1 with sentinel
        let rotated = sibling_path(&path, ".1");
        std::fs::write(&rotated, sentinel).expect("write sentinel .1");

        // Write current log larger than threshold
        std::fs::write(&path, old_content).expect("write current log");

        let max_bytes = (old_content.len() - 1) as u64;
        let _file = rotate_and_open(&path, Some(max_bytes)).expect("rotate and open");

        // .1 should have NEW content (sentinel gone)
        assert_eq!(
            std::fs::read(&rotated).expect("read rotated"),
            old_content,
            "rotated file should have new content, sentinel should be gone"
        );
    }

    #[test]
    fn rotate_and_open_with_none_max_bytes_skips_rotation() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("harnx.log");
        let content = b"some content";

        std::fs::write(&path, content).expect("write initial log");

        // max_bytes = None should skip all rotation logic
        let _file = rotate_and_open(&path, None).expect("rotate and open");

        // No .1, no .lock, file unchanged
        assert!(!sibling_path(&path, ".1").exists(), "no .1 should exist");
        assert!(
            !sibling_path(&path, ".lock").exists(),
            "no .lock should exist"
        );
        assert_eq!(std::fs::read(&path).expect("read"), content);
    }

    #[test]
    fn rotate_if_needed_off_rotates_for_off_level() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("harnx.log");
        let content = b"old content exceeding threshold";

        std::fs::write(&path, content).expect("write initial log");

        let max_bytes = (content.len() - 1) as u64;
        rotate_if_needed_off(&path, max_bytes);

        // Should have rotated
        let rotated = sibling_path(&path, ".1");
        assert!(rotated.exists(), "rotated file should exist");
        assert_eq!(
            std::fs::read(&rotated).expect("read rotated"),
            content,
            "rotated should contain old content"
        );

        // Original path should be gone (renamed away)
        // Note: It may be recreated by the rotation check, but the rotated content is in .1
    }

    #[test]
    fn rotate_and_open_with_contention_does_not_rotate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("harnx.log");
        let lock_path = sibling_path(&path, ".lock");
        let content = b"existing content";

        std::fs::write(&path, content).expect("write initial log");

        // Ensure parent exists for lock
        std::fs::create_dir_all(path.parent().unwrap()).ok();

        // Hold the lock with another handle for the ENTIRE duration of rotate_and_open
        let _held_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("create lock file");
        _held_lock.try_lock().expect("acquire lock");

        // Threshold smaller than file, but lock is held
        let max_bytes = (content.len() - 1) as u64;

        // rotate_and_open should succeed (contended past budget, skips rotation)
        let _file =
            rotate_and_open(&path, Some(max_bytes)).expect("rotate and open with contention");

        // Lock was held by _held_lock throughout, so NO rotation should have occurred
        assert!(
            !sibling_path(&path, ".1").exists(),
            "should NOT have rotated while lock was held"
        );

        // Original content preserved (not renamed away)
        assert_eq!(
            std::fs::read(&path).expect("read path"),
            content,
            "original content should be preserved"
        );

        // Clean up: unlock happens automatically when _held_lock drops
    }

    #[test]
    fn rotate_and_open_with_free_lock_rotates() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("test.log");
        let content = b"content to rotate";

        std::fs::write(&path, content).expect("write initial log");

        let max_bytes = (content.len() - 1) as u64;
        let _file = rotate_and_open(&path, Some(max_bytes)).expect("rotate and open");

        // Should have rotated (lock was free)
        let rotated = sibling_path(&path, ".1");
        assert!(rotated.exists(), "free lock should allow rotation");
        assert_eq!(
            std::fs::read(&rotated).expect("read rotated"),
            content,
            "rotated should contain old content"
        );
    }

    #[test]
    fn rotation_handles_missing_log_file_gracefully() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("nonexistent.log");

        // No file exists
        let _file = rotate_and_open(&path, Some(1000)).expect("rotate and open on missing file");

        // Should create new file
        assert!(path.exists(), "should create new log file");
        assert!(
            !sibling_path(&path, ".1").exists(),
            "no rotation should occur"
        );
    }

    #[test]
    fn rotation_happens_once_in_init_with_not_in_child_path() {
        // Verify child_output_sink uses open_append, not rotate_and_open
        // This is a compile-time property enforced by the code structure,
        // but we can at least verify the function signatures match expectations.

        // child_output_sink should NOT have rotation logic - it just opens
        // Given that the code structure has rotation only in init_with,
        // this test documents the invariant.

        // The actual code path: child_output_sink calls open_append directly.
        // We verify the lock helper is available but not used in child path.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("child.log");
        std::fs::write(&path, b"existing").expect("write");

        // Simulate child path: open_append does NOT rotate
        let _file = open_append(&path).expect("open_append");
        assert!(
            !sibling_path(&path, ".1").exists(),
            "open_append should not rotate"
        );
    }
}

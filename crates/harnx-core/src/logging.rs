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
//!
//! All four are plain env vars, so a child process inherits them and raising the
//! level once raises it for the whole tree.

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
pub fn init_with(settings: LogSettings) -> Result<LogSettings> {
    crate::llm_trace::init_from_env();

    let _ = CURRENT.set(settings.clone());
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

/// Stdio for a child process, following the rule in the module docs: hand the
/// child our log file when we have one, otherwise let it inherit our streams.
///
/// Falls back to [`Stdio::null`] when the file can't be opened. Never `inherit`
/// in that case — the callers that log to a file are the ones drawing on the
/// terminal, and child output there corrupts the display.
///
/// A process that never called [`init`] gets [`Stdio::null`] too, not `inherit`.
/// In practice that means a test harness, whose stdout is a pipe: a child
/// holding an inherited pipe open outlives the test that spawned it and strands
/// the harness waiting on EOF. A process that configured no logging has said
/// nothing about wanting its children's output either.
pub fn child_output_sink() -> Stdio {
    match current().map(|settings| &settings.dest) {
        Some(LogDest::File(path)) => match open_append(path) {
            Ok(file) => Stdio::from(file),
            Err(error) => {
                log::warn!("child output not captured to {}: {error:#}", path.display());
                Stdio::null()
            }
        },
        Some(LogDest::Stderr) => Stdio::inherit(),
        None => Stdio::null(),
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
/// where the giant NUL runs in #880 came from. The file is never truncated per
/// run; rotate or delete it yourself when it grows.
fn open_append(path: &Path) -> Result<std::fs::File> {
    crate::path::ensure_parent_exists(path)?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open log file {}", path.display()))
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

    #[test]
    fn defaults_to_info_text_and_the_harnx_filter() {
        let settings = resolve(env(&[]), LogSink::Stderr, default_path());
        assert_eq!(settings.level, LevelFilter::Info);
        assert_eq!(settings.format, LogFormat::Text);
        assert_eq!(settings.filter, "harnx");
    }

    #[test]
    fn parses_bare_level_names_case_insensitively() {
        assert_eq!(parse_level("debug"), Some(LevelFilter::Debug));
        assert_eq!(parse_level(" WARN "), Some(LevelFilter::Warn));
        assert_eq!(parse_level("off"), Some(LevelFilter::Off));
    }

    #[test]
    fn unrecognised_directives_do_not_silence_the_process() {
        // A per-target RUST_LOG directive must not be read as "off".
        assert_eq!(parse_level("harnx_mcp_bridge=debug"), None);
        assert_eq!(parse_level(""), None);
        let settings = resolve(
            env(&[("RUST_LOG", "harnx_mcp_bridge=debug")]),
            LogSink::Stderr,
            default_path(),
        );
        assert_eq!(settings.level, LevelFilter::Info);
    }

    #[test]
    fn harnx_log_level_wins_over_rust_log() {
        let settings = resolve(
            env(&[("HARNX_LOG_LEVEL", "trace"), ("RUST_LOG", "error")]),
            LogSink::Stderr,
            default_path(),
        );
        assert_eq!(settings.level, LevelFilter::Trace);
    }

    #[test]
    fn rust_log_is_the_fallback_level() {
        let settings = resolve(
            env(&[("RUST_LOG", "debug")]),
            LogSink::Stderr,
            default_path(),
        );
        assert_eq!(settings.level, LevelFilter::Debug);
    }

    #[test]
    fn json_format_is_opt_in_by_name() {
        let json = resolve(
            env(&[("HARNX_LOG_FORMAT", "JSON")]),
            LogSink::Stderr,
            default_path(),
        );
        assert_eq!(json.format, LogFormat::Json);
        let nonsense = resolve(
            env(&[("HARNX_LOG_FORMAT", "logfmt")]),
            LogSink::Stderr,
            default_path(),
        );
        assert_eq!(nonsense.format, LogFormat::Text);
    }

    #[test]
    fn terminal_binaries_default_to_the_state_dir_log_file() {
        let settings = resolve(env(&[]), LogSink::File, default_path());
        assert_eq!(settings.dest, LogDest::File(default_path()));
    }

    #[test]
    fn log_path_overrides_the_default_file() {
        let settings = resolve(
            env(&[("HARNX_LOG_PATH", "/tmp/custom.log")]),
            LogSink::File,
            default_path(),
        );
        assert_eq!(
            settings.dest,
            LogDest::File(PathBuf::from("/tmp/custom.log"))
        );
    }

    #[test]
    fn empty_log_path_falls_back_to_the_default_file() {
        let settings = resolve(
            env(&[("HARNX_LOG_PATH", "")]),
            LogSink::File,
            default_path(),
        );
        assert_eq!(settings.dest, LogDest::File(default_path()));
    }

    #[test]
    fn servers_stay_on_stderr_even_when_log_path_is_inherited() {
        let settings = resolve(
            env(&[("HARNX_LOG_PATH", "/tmp/custom.log")]),
            LogSink::Stderr,
            default_path(),
        );
        assert_eq!(settings.dest, LogDest::Stderr);
        assert_eq!(settings.dest_display(), "stderr");
    }

    #[test]
    fn default_filter_matches_every_harnx_crate_target() {
        // Rust targets use underscores (`harnx_runtime::client`); the historic
        // `harnx::serve` filter matched none of them and dropped every line.
        let settings = resolve(env(&[]), LogSink::Stderr, default_path());
        for target in [
            "harnx_runtime::client",
            "harnx_runtime::bootstrap",
            "harnx_serve::server",
            "harnx_core::logging",
        ] {
            assert!(
                target.starts_with(&settings.filter),
                "{target} should match filter {:?}",
                settings.filter
            );
            assert!(!target.starts_with("harnx::serve"));
        }
    }

    #[test]
    fn log_filter_narrows_to_one_target() {
        let settings = resolve(
            env(&[("HARNX_LOG_FILTER", "harnx_mcp_bridge")]),
            LogSink::Stderr,
            default_path(),
        );
        assert_eq!(settings.filter, "harnx_mcp_bridge");
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
    fn text_lines_carry_level_pid_target_and_message() {
        let line = record_line(LogFormat::Text);
        assert!(line.ends_with('\n'), "{line:?}");
        assert!(line.contains("[INFO ]"), "{line:?}");
        assert!(line.contains(&std::process::id().to_string()), "{line:?}");
        assert!(line.contains("harnx_core::logging: hello 42"), "{line:?}");
        assert!(line.starts_with("20"), "timestamp first: {line:?}");
    }

    #[test]
    fn json_lines_parse_back_with_the_documented_keys() {
        let line = record_line(LogFormat::Json);
        assert!(line.ends_with('\n'), "{line:?}");
        let value: serde_json::Value = serde_json::from_str(line.trim()).expect("valid JSON line");
        assert_eq!(value["level"], "INFO");
        assert_eq!(value["target"], "harnx_core::logging");
        assert_eq!(value["message"], "hello 42");
        assert_eq!(value["pid"].as_u64(), Some(u64::from(std::process::id())));
        assert!(value["ts"].as_str().is_some_and(|ts| ts.ends_with('Z')));
    }

    /// Serialises the tests that mutate process-global env vars. `cargo nextest`
    /// gives each test its own process, but `cargo test` shares one.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn the_default_log_file_is_harnx_log_under_the_state_dir() {
        // Regression test: this path used to be derived from
        // `env!("CARGO_CRATE_NAME")`, so it silently became
        // `harnx_runtime.log` when the logging code moved between crates.
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
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
    fn filter_rejects_foreign_targets_and_levels_above_the_threshold() {
        let logger = HarnxLogger {
            level: LevelFilter::Info,
            format: LogFormat::Text,
            filter: "harnx".to_string(),
            writer: Mutex::new(Box::new(Vec::new())),
        };
        let enabled = |target: &str, level: log::Level| {
            logger.enabled(&Metadata::builder().target(target).level(level).build())
        };
        assert!(enabled("harnx_runtime::client", log::Level::Info));
        assert!(!enabled("harnx_runtime::client", log::Level::Debug));
        assert!(!enabled("hyper::proto", log::Level::Error));
    }
}

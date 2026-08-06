//! Logger setup for the small server binaries (tool servers, hook servers, the
//! MCP bridge).
//!
//! These processes are spawned by a worker with their stdio redirected to the
//! worker log, and they report through the `log` facade. Without a logger
//! installed every one of those calls is a no-op, so the MCP bridge — which
//! forwards its child's stderr line by line to `log::debug!` — silently
//! discarded the only evidence of why a wrapped server would not start.

use simplelog::{ConfigBuilder, LevelFilter, SimpleLogger};

/// Install a stderr logger for a spawned server process.
///
/// Idempotent and infallible: a second call, or a process that already
/// installed its own logger, is a no-op. Servers call this before doing
/// anything that can fail, so startup problems are visible.
///
/// The level comes from `HARNX_LOG_LEVEL`, falling back to `RUST_LOG`, then to
/// `info`. Both are inherited from the worker, so raising the worker's level
/// raises its children's too. The default is `info` rather than `warn` because
/// these processes log to a file, emit few lines, and the failure this exists
/// to fix is a server that says nothing at all about why it never started.
pub fn init_server_logger() {
    let config = ConfigBuilder::new()
        // Restrict to harnx targets, matching the front-end logger. Raising the
        // level otherwise buries the server's own lines under process-spawn and
        // transport chatter from dependencies.
        .add_filter_allow("harnx".to_string())
        .set_thread_level(LevelFilter::Off)
        .build();
    // Errors mean a logger is already installed, which is the desired end state.
    let _ = SimpleLogger::init(server_log_level(), config);
}

fn server_log_level() -> LevelFilter {
    ["HARNX_LOG_LEVEL", "RUST_LOG"]
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .and_then(|value| parse_level(&value))
        .unwrap_or(LevelFilter::Info)
}

/// Accept a bare level name. `RUST_LOG` also supports per-target directives;
/// those are not honoured here, so anything unrecognised keeps the default
/// rather than silencing the process.
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

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}

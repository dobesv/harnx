//! Shared startup helpers used by the `harnx`, `harnx-serve`, and
//! server binaries — logger init, env-file loading, etc.
//! Extracted from `harnx/src/main.rs` so the thin-wrapper bins don't
//! have to duplicate the boilerplate.

use anyhow::Result;
use simplelog::{format_description, ConfigBuilder, LevelFilter, SimpleLogger, WriteLogger};

use crate::config::Config;
use crate::utils::get_env_name;
use harnx_core::path::ensure_parent_exists;

/// Initialise the process-wide `log` facade. Reads log level + path from
/// `Config::log_config` and applies an optional `HARNX_LOG_FILTER` env
/// override. Server mode and CLI/TUI use `harnx`
/// as the default filter, matching all `harnx_*` crate targets via prefix
/// matching (simplelog 0.12 uses `path.starts_with(filter)`). Use
/// `HARNX_LOG_FILTER` to narrow to a specific crate/module.
///
/// # Historical note
///
/// Previously, server modes defaulted to `harnx::serve`, but this matched
/// no `harnx_*` crate targets (underscores vs colons), silently dropping
/// all logs including startup banners and subagent output. Fixed in #989.
pub fn setup_logger(is_server: bool) -> Result<()> {
    // LLM trace is independent of the simplelog filter — it must work even
    // when log_level is Off, since it's the user's primary tool for debugging
    // request/response correctness.
    harnx_core::llm_trace::init_from_env();

    let (log_level, log_path) = Config::log_config(is_server)?;
    if log_level == LevelFilter::Off {
        return Ok(());
    }
    // Hardcode "harnx" — avoids CARGO_CRATE_NAME drift across the 3 bins
    // (harnx and harnx-serve). The crate-name-as-filter
    // trick was fragile anyway; anything that wants crate-specific
    // filtering can override via HARNX_LOG_FILTER.
    const LOG_CRATE_NAME: &str = "harnx";
    let log_filter = match std::env::var(get_env_name("log_filter")) {
        Ok(v) => v,
        Err(_) => default_log_filter(LOG_CRATE_NAME),
    };
    let config = ConfigBuilder::new()
        .add_filter_allow(log_filter)
        .set_time_format_custom(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        ))
        .set_thread_level(LevelFilter::Off)
        .build();
    let log_target = match &log_path {
        Some(p) => p.display().to_string(),
        None => "stderr".to_string(),
    };
    match log_path {
        None => {
            SimpleLogger::init(log_level, config)?;
        }
        Some(log_path) => {
            ensure_parent_exists(&log_path)?;
            // Open in append mode (not truncate). Several harnx processes — the
            // interactive binary plus every child process it
            // spawns — inherit the same `HARNX_LOG_PATH` and write to one file.
            // `File::create` gives each process an independent offset starting
            // at 0, so they clobber each other and the kernel zero-fills the
            // gaps, producing the giant NUL runs in #880. `O_APPEND` makes every
            // write land atomically at EOF, so concurrent writers interleave
            // cleanly at line granularity instead. The file is no longer
            // truncated per run; rotate or delete it yourself when it grows.
            let log_file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)?;
            WriteLogger::init(log_level, config, log_file)?;
        }
    }
    // Stamp every process's startup so a shared log can be attributed per-PID
    // and so the running build is verifiable (which mattered when debugging the
    // #842 OOM: a log with no watchdog lines could mean an old binary). Emitted
    // after init so it lands in the log itself.
    log::info!(
        "harnx start: v{} build={} pid={} level={} log={}",
        env!("CARGO_PKG_VERSION"),
        env!("HARNX_BUILD_SHA"),
        std::process::id(),
        log_level,
        log_target,
    );
    Ok(())
}

/// Returns the default log filter for simplelog.
///
/// Both server and CLI modes use the same default `"harnx"` filter, which
/// matches all `harnx_*` crate targets via simplelog's prefix matching.
fn default_log_filter(crate_name: &str) -> String {
    crate_name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_log_filter_returns_harnx() {
        assert_eq!(default_log_filter("harnx"), "harnx");
    }

    #[test]
    fn harnx_filter_matches_harnx_underscore_targets() {
        // simplelog 0.12 uses path.starts_with(filter) for prefix matching.
        // The fix ensures "harnx" (correct) is used, not "harnx::serve" (bug).
        let filter = default_log_filter("harnx");

        // All harnx_* crate targets should match the "harnx" prefix.
        assert!(
            "harnx_runtime::client".starts_with(&filter),
            "harnx_runtime::client should match filter {:?}",
            filter
        );
        assert!(
            "harnx_mcp::client".starts_with(&filter),
            "harnx_mcp::client should match filter {:?}",
            filter
        );
        assert!(
            "harnx_runtime::bootstrap".starts_with(&filter),
            "harnx_runtime::bootstrap should match filter {:?}",
            filter
        );
        assert!(
            "harnx_serve::server".starts_with(&filter),
            "harnx_serve::server should match filter {:?}",
            filter
        );
    }

    #[test]
    fn harnx_serve_filter_matches_no_harnx_underscore_targets() {
        // Bug regression test: "harnx::serve" matches NONE of the harnx_* targets.
        // simplelog 0.12 uses prefix matching, and underscores != colons.
        let buggy_filter = "harnx::serve";

        assert!(
            !"harnx_runtime::client".starts_with(buggy_filter),
            "harnx_runtime::client should NOT match buggy filter {:?}",
            buggy_filter
        );
        assert!(
            !"harnx_mcp::client".starts_with(buggy_filter),
            "harnx_mcp::client should NOT match buggy filter {:?}",
            buggy_filter
        );
        assert!(
            !"harnx_runtime::bootstrap".starts_with(buggy_filter),
            "harnx_runtime::bootstrap should NOT match buggy filter {:?}",
            buggy_filter
        );
        assert!(
            !"harnx_serve::server".starts_with(buggy_filter),
            "harnx_serve::server should NOT match buggy filter {:?}",
            buggy_filter
        );
    }
}

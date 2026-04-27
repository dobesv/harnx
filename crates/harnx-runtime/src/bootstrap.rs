//! Shared startup helpers used by the `harnx`, `harnx-serve`, and
//! `harnx-acp-server` binaries — logger init, env-file loading, etc.
//! Extracted from `harnx/src/main.rs` so the thin-wrapper bins don't
//! have to duplicate the boilerplate.

use anyhow::Result;
use simplelog::{format_description, ConfigBuilder, LevelFilter, SimpleLogger, WriteLogger};

use crate::config::Config;
use crate::utils::get_env_name;
use harnx_core::path::ensure_parent_exists;

/// Initialise the process-wide `log` facade. Reads log level + path from
/// `Config::log_config` and applies an optional `HARNX_LOG_FILTER` env
/// override. `is_server` is a hint for the default filter: server modes
/// (HTTP serve, ACP) default to `harnx::serve` whereas CLI/TUI default to
/// the top-level `harnx` filter so dot-command logs aren't suppressed.
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
    // (harnx, harnx-serve, harnx-acp-server). The crate-name-as-filter
    // trick was fragile anyway; anything that wants crate-specific
    // filtering can override via HARNX_LOG_FILTER.
    const LOG_CRATE_NAME: &str = "harnx";
    let log_filter = match std::env::var(get_env_name("log_filter")) {
        Ok(v) => v,
        Err(_) => match is_server {
            true => format!("{LOG_CRATE_NAME}::serve"),
            false => LOG_CRATE_NAME.into(),
        },
    };
    let config = ConfigBuilder::new()
        .add_filter_allow(log_filter)
        .set_time_format_custom(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        ))
        .set_thread_level(LevelFilter::Off)
        .build();
    match log_path {
        None => {
            SimpleLogger::init(log_level, config)?;
        }
        Some(log_path) => {
            ensure_parent_exists(&log_path)?;
            let log_file = std::fs::File::create(log_path)?;
            WriteLogger::init(log_level, config, log_file)?;
        }
    }
    Ok(())
}

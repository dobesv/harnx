//! Shared startup helpers used by the `harnx`, `harnx-serve`, and
//! server binaries — logger init, env-file loading, etc.
//! Extracted from `harnx/src/main.rs` so the thin-wrapper bins don't
//! have to duplicate the boilerplate.

use anyhow::Result;
use harnx_core::logging::{self, LogSink};

/// Install the process-wide logger and stamp the startup banner.
///
/// Everything about *where* the records go lives in [`harnx_core::logging`];
/// this wrapper exists for the banner, which needs `HARNX_BUILD_SHA` from this
/// crate's `build.rs`.
pub fn setup_logger(sink: LogSink) -> Result<()> {
    let settings = logging::init(sink)?;
    // Stamp every process's startup so a shared log can be attributed per-PID
    // and so the running build is verifiable (which mattered when debugging the
    // #842 OOM: a log with no watchdog lines could mean an old binary). Emitted
    // after init so it lands in the log itself.
    log::info!(
        "harnx start: v{} build={} pid={} level={} log={}",
        env!("CARGO_PKG_VERSION"),
        env!("HARNX_BUILD_SHA"),
        std::process::id(),
        settings.level,
        settings.dest_display(),
    );
    Ok(())
}

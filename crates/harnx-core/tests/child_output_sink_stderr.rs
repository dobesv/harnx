//! Installing a stderr logger must not open a log file — not even when
//! `HARNX_LOG_PATH` is set, which every server inherits from the front-end that
//! started it. Two writers on one file is what this design exists to avoid.
//!
//! Its own test binary: `logging::init` publishes to a process-global
//! `OnceLock`, and the file case is asserted by `child_output_sink_file.rs`.

use harnx_core::logging::{self, LogDest, LogSink};

#[test]
fn a_stderr_logging_process_never_opens_a_log_file() {
    let dir = tempfile::tempdir().expect("temp state dir");
    let forbidden = dir.path().join("must-not-be-written.log");
    // This test binary holds one test, so nothing else reads the environment
    // concurrently.
    std::env::set_var("HARNX_STATE_DIR", dir.path());
    std::env::set_var("HARNX_LOG_PATH", &forbidden);
    std::env::set_var("HARNX_LOG_LEVEL", "info");

    let settings = logging::init(LogSink::Stderr).expect("install logger");
    assert_eq!(settings.dest, LogDest::Stderr);
    assert_eq!(logging::log_file_path(), None);

    log::info!(target: "harnx_core::test", "this goes to stderr");
    assert!(
        !forbidden.exists(),
        "a stderr-logging process must not open {}",
        forbidden.display()
    );
}

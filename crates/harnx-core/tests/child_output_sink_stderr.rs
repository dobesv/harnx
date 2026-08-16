//! A process that logs to stderr lets its children inherit its streams, and
//! never opens a log file — not even when `HARNX_LOG_PATH` is set, which every
//! server inherits from the front-end that started it.
//!
//! Its own test binary: see `child_output_sink_file.rs`.

use std::process::Command;

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

    // The child inherits our streams, so its line lands in the test harness's
    // own output rather than anywhere we can capture. What matters is that
    // nothing opened the inherited HARNX_LOG_PATH. (This test installs a logger,
    // so it takes the `inherit` branch; a process that never calls `init` gets
    // `Stdio::null` instead, so a test harness's pipe is never held open.)
    let status = Command::new("sh")
        .args(["-c", "echo a line from an inheriting child"])
        .stdout(logging::child_output_sink())
        .stderr(logging::child_output_sink())
        .status()
        .expect("run child");
    assert!(status.success());
    assert!(
        !forbidden.exists(),
        "a stderr-logging process must not open {}",
        forbidden.display()
    );
}

//! A process that logs to a file hands its children that same file.
//!
//! Lives in its own test binary because `logging::init` publishes to a
//! process-global `OnceLock`: the stderr half of this rule is asserted by
//! `child_output_sink_stderr.rs`, which needs a fresh process.

use std::process::Command;

use harnx_core::logging::{self, LogSink};

#[test]
fn a_file_logging_process_hands_children_its_log_file() {
    let dir = tempfile::tempdir().expect("temp state dir");
    // This test binary holds one test, so nothing else reads the environment
    // concurrently.
    std::env::set_var("HARNX_STATE_DIR", dir.path());
    std::env::remove_var("HARNX_LOG_PATH");
    std::env::remove_var("XDG_STATE_HOME");
    std::env::set_var("HARNX_LOG_LEVEL", "info");

    let settings = logging::init(LogSink::File).expect("install logger");
    let log_path = dir.path().join("harnx.log");
    assert_eq!(
        logging::log_file_path(),
        Some(log_path.as_path()),
        "resolved {}",
        settings.dest_display()
    );

    log::info!(target: "harnx_core::test", "a line from the parent");
    // This crate's own target doesn't start with the default `harnx` filter, so
    // it must be dropped.
    log::info!("a line from a foreign target");
    let status = Command::new("sh")
        .args([
            "-c",
            "echo a line from the child; echo and one on stderr >&2",
        ])
        .stdout(logging::child_output_sink())
        .stderr(logging::child_output_sink())
        .status()
        .expect("run child");
    assert!(status.success());

    let body = std::fs::read_to_string(&log_path).expect("read log");
    assert!(body.contains("a line from the parent"), "{body}");
    assert!(!body.contains("a foreign target"), "{body}");
    assert!(body.contains("a line from the child"), "{body}");
    assert!(body.contains("and one on stderr"), "{body}");
}

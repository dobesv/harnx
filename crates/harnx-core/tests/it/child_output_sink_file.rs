//! A process that logs to a file really does get its children's bytes into that
//! file — the end-to-end half of the rule `logging::child_output` decides.
//!
//! Lives in its own test binary because `logging::init` publishes to a
//! process-global `OnceLock` that this test must be the one to set.

use std::process::Command;

use harnx_core::logging::{self, LogSink};

/// A child that writes `text` to `stream`, via whichever shell the platform is
/// guaranteed to have. `sh` is not dependable on Windows.
fn echo_to(stream: &str, text: &str) -> Command {
    let redirect = if stream == "stderr" { " 1>&2" } else { "" };
    let script = format!("echo {text}{redirect}");
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", &script]);
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = Command::new("sh");
        command.args(["-c", &script]);
        command
    };
    command
        .stdout(logging::child_output_sink())
        .stderr(logging::child_output_sink());
    command
}

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

    for (stream, text) in [
        ("stdout", "child-stdout-line"),
        ("stderr", "child-stderr-line"),
    ] {
        let status = echo_to(stream, text)
            .status()
            .unwrap_or_else(|error| panic!("spawn child writing to {stream}: {error}"));
        assert!(
            status.success(),
            "child writing to {stream} exited {status}"
        );
    }

    let body = std::fs::read_to_string(&log_path).expect("read log");
    assert!(body.contains("a line from the parent"), "{body}");
    assert!(!body.contains("a foreign target"), "{body}");
    assert!(body.contains("child-stdout-line"), "{body}");
    assert!(body.contains("child-stderr-line"), "{body}");
}

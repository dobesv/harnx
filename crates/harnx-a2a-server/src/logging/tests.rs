use super::*;
use std::sync::Arc;

const TARGET: &str = "harnx_a2a_server::requests";

fn capture(level: Option<&str>, filter: &str, format: &str, rust_log: &str) -> String {
    harnx_core::require_nextest();
    match level {
        Some(level) => std::env::set_var("HARNX_LOG_LEVEL", level),
        None => std::env::remove_var("HARNX_LOG_LEVEL"),
    }
    std::env::set_var("HARNX_LOG_FILTER", filter);
    std::env::set_var("HARNX_LOG_FORMAT", format);
    std::env::set_var("RUST_LOG", rust_log);
    let file = tempfile::NamedTempFile::new().unwrap();
    let writer = Arc::new(file.reopen().unwrap());
    let subscriber = subscriber(&harnx_core::logging::settings(LogSink::Stderr), writer).unwrap();
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: TARGET, method = "GetTask", task_id = "task", duration_ms = 2.5, "A2A request finished");
        tracing::debug!(target: TARGET, user_id = "private-caller", "A2A caller resolved");
        tracing::info!(target: "harnx_a2a_server::other", "unrelated event");
        tracing::info!(target: "external", "third party event");
    });
    std::fs::read_to_string(file.path()).unwrap()
}

#[test]
fn harnx_logging_json_preserves_fields_and_filters_targets_and_identity() {
    let output = capture(Some("info"), TARGET, "json", "trace");
    let lines: Vec<serde_json::Value> = output
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 1, "{output}");
    let event = &lines[0];
    assert_eq!(event["target"], TARGET);
    assert_eq!(event["fields"]["method"], "GetTask");
    assert_eq!(event["fields"]["task_id"], "task");
    assert_eq!(event["fields"]["duration_ms"], 2.5);
    assert!(!output.contains("private-caller"));
}

#[test]
fn harnx_logging_debug_identity_and_text_format() {
    let output = capture(Some("debug"), TARGET, "text", "off");
    assert_eq!(output.lines().count(), 2, "{output}");
    assert!(output.contains("A2A request finished"));
    assert!(output.contains("private-caller"));
    assert!(!output.contains("unrelated event"));
}

#[test]
fn harnx_logging_rust_log_fallback_accepts_only_bare_levels() {
    assert!(capture(None, TARGET, "text", "warn").is_empty());
    let output = capture(None, TARGET, "text", "debug");
    assert!(output.contains("private-caller"));
    let output = capture(None, TARGET, "text", "harnx_a2a_server=trace");
    assert!(output.contains("A2A request finished"));
    assert!(!output.contains("private-caller"));
    assert!(capture(Some("off"), TARGET, "json", "trace").is_empty());
}

#[test]
fn harnx_logging_init_keeps_shared_logger_and_tracing_subscriber() {
    harnx_core::require_nextest();
    init().unwrap();
    assert!(harnx_core::logging::current().is_some());
    assert!(tracing::dispatcher::has_been_set());
}

fn clear_logging_env() {
    harnx_core::require_nextest();
    for name in [
        "RUST_LOG",
        "HARNX_LOG_LEVEL",
        "HARNX_LOG_FILTER",
        "HARNX_LOG_FORMAT",
        "HARNX_LOG_PATH",
        "HARNX_LOG_MAX_BYTES",
    ] {
        std::env::remove_var(name);
    }
}

fn legacy_warning_output() -> String {
    let file = tempfile::NamedTempFile::new().unwrap();
    let writer = Arc::new(file.reopen().unwrap());
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::WARN)
        .with_writer(writer)
        .finish();
    tracing::subscriber::with_default(subscriber, warn_legacy_rust_log);
    std::fs::read_to_string(file.path()).unwrap()
}

#[test]
fn harnx_logging_warns_once_for_ignored_rust_log_without_echoing_value() {
    clear_logging_env();
    for value in [
        "harnx_a2a_server=debug,hyper=warn",
        "private-log-directive=off",
    ] {
        std::env::set_var("RUST_LOG", value);
        let output = legacy_warning_output();
        let events: Vec<serde_json::Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 1, "{output}");
        assert_eq!(events[0]["level"], "WARN");
        assert!(events[0]["fields"]["message"]
            .as_str()
            .unwrap()
            .contains("HARNX_LOG_LEVEL and HARNX_LOG_FILTER"));
        assert!(!output.contains(value));
    }
}

#[test]
fn harnx_logging_skips_migration_warning_for_bare_levels_or_harnx_settings() {
    clear_logging_env();
    assert!(legacy_warning_output().is_empty());
    for level in [
        "", " ", "off", "error", "warn", "info", "debug", "trace", " DEBUG ",
    ] {
        std::env::set_var("RUST_LOG", level);
        assert!(legacy_warning_output().is_empty(), "{level}");
    }
    std::env::set_var("RUST_LOG", "harnx_a2a_server=debug");
    for name in [
        "HARNX_LOG_LEVEL",
        "HARNX_LOG_FILTER",
        "HARNX_LOG_FORMAT",
        "HARNX_LOG_PATH",
        "HARNX_LOG_MAX_BYTES",
    ] {
        std::env::set_var(name, "");
        assert!(legacy_warning_output().is_empty(), "{name}");
        std::env::remove_var(name);
    }
}

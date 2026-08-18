#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use harnx_core::hooks::{HookEvent, HookPayload, HARNX_HOOK_PROTOCOL_JSONL};
use harnx_hooks::{HookCommand, PersistentHookManager};
use log::{Level, LevelFilter, Log, Metadata, Record};

fn temp_test_dir(name: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("unix epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("harnx-persistent-protocol-{name}-{suffix}"));
    fs::create_dir_all(&dir).expect("create temp test dir");
    dir
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(format!("{name}.sh"));
    fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).expect("write hook script");
    let mut permissions = fs::metadata(&path).expect("script metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("set script permissions");
    path
}

fn hook_command(path: &Path) -> HookCommand {
    HookCommand {
        argv: vec![path.display().to_string()],
        timeout: Some(5),
        package_dir: None,
    }
}

fn test_payload(cwd: &Path) -> HookPayload {
    HookPayload {
        session_id: "session-123".to_string(),
        cwd: cwd.to_path_buf(),
        resume_count: 0,
        hook_event: HookEvent::Stop {
            stop_hook_active: false,
            last_assistant_message: Some("done".to_string()),
        },
    }
}

#[tokio::test]
async fn persistent_process_declares_jsonl_protocol() {
    let dir = temp_test_dir("jsonl");
    let script = write_script(
        &dir,
        "protocol",
        r#"while IFS= read -r line; do
  id=${line#*\"id\":\"}; id=${id%%\"*}
  printf '{"id":"%s","additionalContext":"%s"}\n' "$id" "$HARNX_HOOK_PROTOCOL"
done"#,
    );
    let mut manager = PersistentHookManager::new();

    let outcome = manager
        .send_event(&test_payload(&dir), &hook_command(&script))
        .await;

    assert_eq!(
        outcome.result.additional_context.as_deref(),
        Some(HARNX_HOOK_PROTOCOL_JSONL)
    );
    manager.shutdown();
    fs::remove_dir_all(dir).expect("remove temp test dir");
}

struct RecordingLogger {
    records: Arc<Mutex<Vec<(Level, String)>>>,
}

impl Log for RecordingLogger {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        self.records
            .lock()
            .expect("lock log records")
            .push((record.level(), record.args().to_string()));
    }

    fn flush(&self) {}
}

#[tokio::test]
async fn healthy_persistent_hook_stderr_is_debug_only() {
    harnx_core::require_nextest();

    let records = Arc::new(Mutex::new(Vec::new()));
    log::set_boxed_logger(Box::new(RecordingLogger {
        records: Arc::clone(&records),
    }))
    .expect("install test logger");
    log::set_max_level(LevelFilter::Trace);

    let marker = "benign-persistent-hook-startup";
    let dir = temp_test_dir("stderr");
    let script = write_script(
        &dir,
        "stderr",
        &format!(
            r#"printf '%s\n' '{marker}' >&2
while IFS= read -r line; do
  id=${{line#*\"id\":\"}}; id=${{id%%\"*}}
  printf '{{"id":"%s"}}\n' "$id"
done"#
        ),
    );
    let mut manager = PersistentHookManager::new();

    manager
        .send_event(&test_payload(&dir), &hook_command(&script))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let records = records.lock().expect("lock log records");
    assert!(
        records
            .iter()
            .any(|(level, message)| *level == Level::Debug && message.contains(marker)),
        "stderr diagnostic was not logged at debug: {records:?}"
    );
    assert!(
        !records.iter().any(|(level, message)| {
            matches!(*level, Level::Error | Level::Warn) && message.contains(marker)
        }),
        "healthy hook stderr was promoted to a warning: {records:?}"
    );
    drop(records);
    manager.shutdown();
    fs::remove_dir_all(dir).expect("remove temp test dir");
}

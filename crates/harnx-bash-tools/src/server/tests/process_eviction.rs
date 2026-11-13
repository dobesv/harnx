use super::process_wait::status_params;
use super::*;
use std::future::{poll_fn, Future};
use std::task::Poll;
use std::time::Instant;

/// Synthetic supervisor channel: tests choose exit times without sleeping.
async fn insert_process(
    server: &BashServer,
    directory: &Path,
    id: &str,
) -> watch::Sender<Option<ProcessExit>> {
    let logs = server.inner.log_dir.join(id);
    std::fs::create_dir_all(&logs).unwrap();
    let stdout_log_path = logs.join("stdout.log");
    let stderr_log_path = logs.join("stderr.log");
    std::fs::write(&stdout_log_path, "completed stdout\n").unwrap();
    std::fs::write(&stderr_log_path, "completed stderr\n").unwrap();
    let (exit_tx, exit) = watch::channel(None);
    let (control, _control_rx) = mpsc::unbounded_channel();
    server
        .store_spawned_process(
            id.to_owned(),
            SpawnedProcess {
                command: "synthetic process".to_owned(),
                working_dir: directory.to_owned(),
                stdout_log_path,
                stderr_log_path,
                before_snap_ids: Vec::new(),
                snapshot_decision: SnapshotDecision::ReadOnly,
                started_at: Instant::now() - Duration::from_secs(3600),
                deadline_secs: 86400,
                exit,
                control,
                exit_diff: Arc::default(),
            },
        )
        .await;
    exit_tx
}

fn exited(at: Instant) -> ProcessExit {
    ProcessExit {
        code: Some(7),
        exited_at: Some(at),
        timed_out: false,
        wait_error: None,
    }
}

async fn lookup_errors(server: &BashServer, id: &str) -> Vec<ErrorData> {
    vec![
        server.wait_impl(wait_params(id, 1)).await.unwrap_err(),
        server.status_impl(status_params(id)).await.unwrap_err(),
        server
            .terminate_impl(TerminateParams {
                execution_id: id.to_owned(),
                signal: None,
            })
            .await
            .unwrap_err(),
    ]
}

#[tokio::test]
async fn ttl_evicts_from_supervisor_exit_time_and_releases_cached_diff() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let before_spawn = Instant::now();
    let result = server
        .spawn_impl(spawn_params("echo completed", directory.path()))
        .await
        .unwrap();
    let id = extract_field(&text_content(&result), "execution_id");
    let entry = server.get_spawned_entry(&id).await.unwrap();
    let mut exit = entry.exit.clone();
    let observed = tokio::time::timeout(Duration::from_secs(5), exit.wait_for(Option::is_some))
        .await
        .unwrap()
        .unwrap()
        .clone()
        .unwrap();
    let at = observed.exited_at.unwrap();
    assert!(at >= before_spawn && at <= Instant::now());
    entry.exit_diff.set(vec!["cached diff".to_owned()]).unwrap();
    let weak_entry = Arc::downgrade(&entry);
    let weak_diff = Arc::downgrade(&entry.exit_diff);
    drop(entry);

    let mut spawned = server.inner.spawned.lock().await;
    let ttl = spawned.exited_ttl;
    spawned.evict(at + ttl - Duration::from_nanos(1));
    assert!(spawned.entries.contains_key(&id));
    spawned.evict(at + ttl);
    assert!(!spawned.entries.contains_key(&id));
    assert!(spawned.was_evicted(&id));
    assert!(weak_entry.upgrade().is_none());
    assert!(weak_diff.upgrade().is_none());
}

#[tokio::test]
async fn cap_evicts_oldest_exits_not_oldest_spawns_on_new_spawn() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    server.inner.spawned.lock().await.max_exited = 2;
    let now = Instant::now();
    // Insertion order differs from exit order. No wait/status observes these exits.
    for (id, age) in [
        ("exec-newest", 0),
        ("exec-oldest", 3),
        ("exec-newer", 1),
        ("exec-older", 2),
    ] {
        let exit = insert_process(&server, directory.path(), id).await;
        exit.send_replace(Some(exited(now - Duration::from_secs(age))));
    }
    let _running = insert_process(&server, directory.path(), "exec-running").await;
    let spawned = server.inner.spawned.lock().await;
    assert_eq!(spawned.entries.len(), 3);
    assert!(spawned.entries.contains_key("exec-newest"));
    assert!(spawned.entries.contains_key("exec-newer"));
    assert!(spawned.entries.contains_key("exec-running"));
    assert!(spawned.was_evicted("exec-oldest"));
    assert!(spawned.was_evicted("exec-older"));
}

#[tokio::test]
async fn running_and_unknown_fate_processes_are_never_evicted() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let _running = insert_process(&server, directory.path(), "exec-running").await;
    let failed = insert_process(&server, directory.path(), "exec-failed-wait").await;
    failed.send_replace(Some(ProcessExit {
        code: None,
        exited_at: None,
        timed_out: false,
        wait_error: Some("injected wait failure".to_owned()),
    }));
    let mut spawned = server.inner.spawned.lock().await;
    spawned.exited_ttl = Duration::ZERO;
    spawned.max_exited = 0;
    spawned.evict(Instant::now() + Duration::from_secs(86400));
    assert_eq!(spawned.entries.len(), 2);
    assert!(!spawned.was_evicted("exec-running"));
    assert!(!spawned.was_evicted("exec-failed-wait"));
}

#[tokio::test]
async fn lookup_evicts_expired_entries_and_reports_finished_id() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let exit = insert_process(&server, directory.path(), "exec-finished").await;
    server.inner.spawned.lock().await.exited_ttl = Duration::ZERO;
    exit.send_replace(Some(exited(Instant::now())));
    for error in lookup_errors(&server, "exec-finished").await {
        assert_eq!(
            error.message,
            "execution_id 'exec-finished' finished and is no longer tracked"
        );
    }
    assert!(server.inner.spawned.lock().await.entries.is_empty());
}

#[tokio::test]
async fn unknown_id_keeps_existing_error_message() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    for error in lookup_errors(&server, "exec-unknown").await {
        assert_eq!(
            error.message,
            "execution_id 'exec-unknown' is not a tracked background process"
        );
    }
}

#[tokio::test]
async fn evicted_id_history_is_bounded_and_forgets_oldest_evictions() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    {
        let mut spawned = server.inner.spawned.lock().await;
        spawned.exited_ttl = Duration::ZERO;
        spawned.max_evicted_ids = 2;
    }
    for index in 0..5 {
        let id = format!("exec-{index}");
        let exit = insert_process(&server, directory.path(), &id).await;
        exit.send_replace(Some(exited(Instant::now())));
        assert!(server.get_spawned_entry(&id).await.is_err());
    }
    let spawned = server.inner.spawned.lock().await;
    assert!(spawned.entries.is_empty());
    for index in 0..5 {
        assert_eq!(spawned.was_evicted(&format!("exec-{index}")), index >= 3);
    }
    drop(spawned);
    for error in lookup_errors(&server, "exec-0").await {
        assert_eq!(
            error.message,
            "execution_id 'exec-0' is not a tracked background process"
        );
    }
}

#[tokio::test]
async fn unrelated_tool_call_evicts_exits_without_removing_logs() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let exit = insert_process(&server, directory.path(), "exec-finished").await;
    server.inner.spawned.lock().await.exited_ttl = Duration::ZERO;
    exit.send_replace(Some(exited(Instant::now())));
    let result = server
        .invoke_tool_value(
            "read_exec_log",
            json!({"execution_id": "exec-finished", "stream": "stdout"}),
        )
        .await
        .unwrap();
    assert!(text_content(&result).contains("completed stdout"));
    assert!(server.inner.spawned.lock().await.entries.is_empty());
}

#[tokio::test]
async fn in_flight_wait_returns_full_result_after_entry_eviction() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let exit = insert_process(&server, directory.path(), "exec-waiting").await;
    let entry = server.get_spawned_entry("exec-waiting").await.unwrap();
    entry
        .exit_diff
        .set(vec!["cached exit diff".to_owned()])
        .unwrap();
    let weak_entry = Arc::downgrade(&entry);
    let weak_diff = Arc::downgrade(&entry.exit_diff);
    drop(entry);
    let mut wait = Box::pin(server.wait_impl(wait_params("exec-waiting", 5)));
    poll_fn(|cx| {
        assert!(wait.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    // The pending wait has already cloned the entry from the registry.
    assert_eq!(weak_entry.strong_count(), 2);
    let at = Instant::now();
    exit.send_replace(Some(exited(at)));
    {
        let mut spawned = server.inner.spawned.lock().await;
        let ttl = spawned.exited_ttl;
        spawned.evict(at + ttl);
        assert!(spawned.entries.is_empty());
    }
    assert_eq!(weak_entry.strong_count(), 1);
    let result = wait.await.unwrap();
    let text = text_content(&result);
    assert_eq!(extract_field(&text, "status"), "exited");
    assert_eq!(extract_field(&text, "exit_code"), "7");
    assert!(text.contains("completed stdout"));
    assert!(text.contains("completed stderr"));
    assert!(text.contains("cached exit diff"));
    assert!(weak_entry.upgrade().is_none());
    assert!(weak_diff.upgrade().is_none());
}

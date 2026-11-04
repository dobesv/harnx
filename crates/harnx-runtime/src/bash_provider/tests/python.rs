use super::*;

#[tokio::test]
async fn python_processes_tool_output_from_vfs_without_tool_bridge() {
    let inner = FakeProvider::new();
    let (bash, _slot, hooks, _ctx) = harness(inner.clone(), vec![]);
    let output = ok(run(
        &bash,
        "fs_read --path a > /tmp/result; python3 - <<'EOF'\nprint(open('/tmp/result').read().strip())\nprint(sum(range(10)))\nEOF",
        &create_abort_signal(),
    ).await);
    assert_eq!(output.value["stdout"], "fs_read:a\n45\n");
    assert_eq!(output.value["status"], "ok");
    assert_eq!(output.value["trace"].as_array().unwrap().len(), 1);
    let error = recoverable(
        run(
            &bash,
            "python3 -c 'fs_read(path=\"a\")'",
            &create_abort_signal(),
        )
        .await,
        "Python tool call",
    );
    let partial = partial_result_of(&error).unwrap();
    assert_eq!(partial["status"], "error");
    assert!(
        partial["stderr"]
            .as_str()
            .unwrap()
            .contains("no external function handler configured"),
        "{partial}"
    );
    assert!(partial["trace"].as_array().unwrap().is_empty());
    assert_eq!(inner.calls.lock().unwrap().len(), 1);
    assert_eq!(
        hooks
            .lock()
            .unwrap()
            .iter()
            .filter(|hook| matches!(hook, HookEvent::PreToolUse { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn python_timeout_keeps_partial_output_and_limit_metadata() {
    let (bash, _slot, _hooks, _ctx) = harness(FakeProvider::new(), vec![]);
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        bash.call_tool(
            BASH_TOOL_NAME,
            json!({"script": "fs_read --path a; python3 -c 'while True: pass'", "timeout_secs": 1}),
            &create_abort_signal(),
        ),
    )
    .await
    .expect("Python hung past run timeout");
    let error = recoverable(result, "Python timeout");
    let partial = partial_result_of(&error).unwrap();
    assert_eq!(partial["status"], "timed_out", "{partial}");
    assert_eq!(partial["exit_code"], 124);
    assert!(!partial["limit_hit"].is_null(), "{partial}");
    assert_eq!(partial["error"], partial["limit_hit"], "{partial}");
    assert!(partial["stdout"].as_str().unwrap().contains("fs_read:a"));
    assert_eq!(partial["trace"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn python_memory_limit_is_recoverable_with_clear_partial_stderr() {
    let (bash, _slot, _hooks, _ctx) = harness(FakeProvider::new(), vec![]);
    let error = recoverable(
        run(
            &bash,
            "echo before; python3 -c 'x = [0] * 100000000'; echo after",
            &create_abort_signal(),
        )
        .await,
        "Python memory limit",
    );
    let partial = partial_result_of(&error).unwrap();
    assert_eq!(partial["status"], "error");
    assert_eq!(partial["exit_code"], 1);
    assert_eq!(partial["stdout"], "before\n");
    assert!(
        partial["stderr"]
            .as_str()
            .unwrap()
            .contains("MemoryError: memory limit exceeded"),
        "{partial}"
    );
}

struct LoopEntry(tokio::sync::Semaphore);

#[async_trait::async_trait]
impl harnx_script::fs_attachments::AttachmentFetcher for LoopEntry {
    async fn fetch(&self, _cid: &str, _max_bytes: u64) -> std::io::Result<Vec<u8>> {
        self.0.add_permits(1);
        Ok(vec![1])
    }

    async fn size(&self, _cid: &str) -> std::io::Result<Option<u64>> {
        Ok(Some(1))
    }
}

#[tokio::test]
async fn python_parent_abort_stops_busy_loop_within_one_second() {
    let entered = Arc::new(LoopEntry(tokio::sync::Semaphore::new(0)));
    let (bash, _slot, _hooks, _ctx) =
        harness_with_attachments(FakeProvider::new(), vec![], Some(entered.clone()));
    let abort = create_abort_signal();
    let task_abort = abort.clone();
    let task = tokio::spawn(async move {
        run(
            &bash,
            "fs_read --path a; python3 - <<'EOF'\nfirst = True\nwhile True:\n if first:\n  first = False\n  open('/attachments/cid:media:agent/session/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa').read()\nEOF",
            &task_abort,
        )
        .await
    });
    // This VFS read occurs inside the first loop iteration, not before Python.
    tokio::time::timeout(Duration::from_secs(2), entered.0.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let start = std::time::Instant::now();
    abort.set_ctrlc();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("Python ignored parent abort")
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    let error = fatal(result, "parent abort during Python");
    let partial = partial_result_of(&error).unwrap();
    assert_eq!(partial["status"], "cancelled", "{partial}");
    assert_eq!(partial["exit_code"], 130);
    assert_eq!(partial["trace"].as_array().unwrap().len(), 1);
}

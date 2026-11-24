use super::*;
use harnx_toolset::ToolProgressKind;
use std::path::Path;

fn command(mode: &str) -> Value {
    let python = which::which("python3")
        .or_else(|_| which::which("python"))
        .expect("Python installed");
    json!([
        python,
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hook.py"),
        mode
    ])
}

fn arguments(mode: &str) -> Value {
    json!({"command":command(mode), "persistent":mode == "--persistent", "event":"PreToolUse",
        "payload":{"session_id":"session", "cwd":std::env::current_dir().unwrap(), "tool_name":"bash_exec",
            "tool_use_id":"call", "tool_input":{"command":"original"}}})
}

async fn run(toolset: &ClaudeHookToolset, args: Value) -> Result<Value, ToolInvokeError> {
    toolset.invoke("run", args, CancellationToken::new()).await
}

#[test]
fn spec_declares_execution_and_no_replay_hints() {
    let toolset = ClaudeHookToolset::default();
    assert_eq!(toolset.name(), "claude_hook");
    assert_eq!(toolset.default_mcp_http_port(), 3008);
    let spec = toolset.tools().remove(0);
    assert_eq!(spec.kind(), Some(ToolProgressKind::Execute));
    assert!(!spec.idempotent_hint);
    assert!(!spec.read_only_hint);
    assert_eq!(spec.timeout_secs, Some(0));
}

#[tokio::test]
async fn one_shot_deny_and_mutation_match_claude_protocol() {
    let toolset = ClaudeHookToolset::default();
    for (mode, reason) in [
        ("exit2", "blocked by exit 2"),
        ("deny", "blocked by policy"),
    ] {
        let result = run(&toolset, arguments(mode)).await.unwrap();
        assert_eq!(result["decision"], "deny");
        assert_eq!(result["reason"], reason);
    }
    let result = run(&toolset, arguments("mutate")).await.unwrap();
    assert_eq!(result["decision"], "allow");
    assert_eq!(result["tool_input"]["command"], "rewritten");
    assert_eq!(result["context"], json!(["pre context"]));
    assert_eq!(result["result"]["diagnostics"]["count"], 1);
    let mut args = arguments("mutate");
    args["event"] = json!("PostToolUse");
    args["payload"]["tool_response"] = json!({"ok":true});
    let result = run(&toolset, args).await.unwrap();
    assert_eq!(result["tool_response"], json!({"post":{"ok":true}}));
    assert_eq!(result["context"], json!(["post context"]));
}

#[tokio::test]
async fn jsonl_reuses_process_and_correlates_serial_and_concurrent_requests() {
    let toolset = ClaudeHookToolset::default();
    let args = arguments("--persistent");
    let first = run(&toolset, args.clone()).await.unwrap();
    let second = run(&toolset, args.clone()).await.unwrap();
    assert_eq!(first["result"]["diagnostics"]["count"], 1);
    assert_eq!(second["result"]["diagnostics"]["count"], 2);
    assert_ne!(
        first["result"]["diagnostics"]["id"],
        second["result"]["diagnostics"]["id"]
    );
    assert_eq!(second["result"]["diagnostics"]["protocol"], "jsonl");
    let (third, fourth) = tokio::join!(run(&toolset, args.clone()), run(&toolset, args));
    let mut counts = [
        third.unwrap()["result"]["diagnostics"]["count"]
            .as_u64()
            .unwrap(),
        fourth.unwrap()["result"]["diagnostics"]["count"]
            .as_u64()
            .unwrap(),
    ];
    counts.sort();
    assert_eq!(counts, [3, 4]);
}

#[tokio::test]
async fn cwd_package_session_and_extra_fields_are_preserved_or_partitioned() {
    let toolset = ClaudeHookToolset::default();
    let cwd = tempfile::tempdir().unwrap();
    let package = tempfile::tempdir().unwrap();
    for mode in ["mutate", "--persistent"] {
        let mut args = arguments(mode);
        args["payload"]["cwd"] = json!(cwd.path());
        args["payload"]["extra"] = json!({"opaque":true});
        args["package_dir"] = json!(package.path());
        let result = run(&toolset, args.clone()).await.unwrap();
        let diagnostics = &result["result"]["diagnostics"];
        assert_eq!(
            Path::new(diagnostics["cwd"].as_str().unwrap())
                .canonicalize()
                .unwrap(),
            cwd.path().canonicalize().unwrap()
        );
        assert_eq!(diagnostics["package_dir"], json!(package.path()));
        assert_eq!(diagnostics["extra"], json!({"opaque":true}));
        args["payload"]["session_id"] = json!("other");
        assert_eq!(
            run(&toolset, args).await.unwrap()["result"]["diagnostics"]["count"],
            1
        );
    }
}

#[tokio::test]
async fn validation_rejects_bad_requests_before_spawning() {
    let toolset = ClaudeHookToolset::default();
    for (key, value) in [
        ("command", json!([])),
        ("command", json!([""])),
        ("payload", json!([])),
        ("event", json!("Unknown")),
        ("timeout_secs", json!(0)),
        ("timeout_secs", json!(301)),
        ("unknown", json!(true)),
    ] {
        let mut args = arguments("mutate");
        args[key] = value;
        assert!(run(&toolset, args).await.is_err(), "{key}");
    }
    for (key, value) in [
        ("id", json!("forged")),
        ("hook_event_name", json!("Stop")),
        ("cwd", json!("relative")),
        ("cwd", json!(false)),
    ] {
        let mut args = arguments("mutate");
        args["payload"][key] = value;
        assert!(run(&toolset, args).await.is_err(), "{key}");
    }
    let mut args = arguments("mutate");
    args["payload"]["extra"] = json!("x".repeat(MAX_BYTES));
    assert!(run(&toolset, args)
        .await
        .unwrap_err()
        .to_string()
        .contains("request exceeds"));
}

#[tokio::test]
async fn process_and_protocol_errors_do_not_silently_allow() {
    let toolset = ClaudeHookToolset::default();
    for mode in ["exit1", "broken"] {
        assert!(run(&toolset, arguments(mode)).await.is_err());
    }
    let mut args = arguments("mutate");
    args["command"] = json!(["harnx-nonexistent-claude-hook"]);
    assert!(run(&toolset, args).await.is_err());
    assert_eq!(
        run(&toolset, arguments("text")).await.unwrap()["context"],
        json!(["plain context"])
    );
    let mut args = arguments("--persistent");
    args["payload"]["tool_input"]["action"] = json!("wrong_id");
    assert!(run(&toolset, args)
        .await
        .unwrap_err()
        .to_string()
        .contains("id does not match"));
    // A bad exchange retires its process; it never retries that request.
    assert_eq!(
        run(&toolset, arguments("--persistent")).await.unwrap()["result"]["diagnostics"]["count"],
        1
    );
}

#[tokio::test]
async fn bounded_stdout_and_noisy_stderr_do_not_deadlock() {
    let toolset = ClaudeHookToolset::default();
    for mode in ["oversize", "--persistent"] {
        let mut args = arguments(mode);
        args["payload"]["tool_input"]["action"] = json!("oversize");
        assert!(run(&toolset, args)
            .await
            .unwrap_err()
            .to_string()
            .contains("exceeds"));
    }
    for mode in ["stderr", "--persistent"] {
        let mut args = arguments(mode);
        args["payload"]["tool_input"]["action"] = json!("stderr");
        assert_eq!(run(&toolset, args).await.unwrap()["decision"], "allow");
    }
    let mut args = arguments("--persistent");
    args["payload"]["tool_input"]["action"] = json!("notice");
    assert_eq!(run(&toolset, args).await.unwrap()["decision"], "allow");
}

#[tokio::test]
async fn timeout_retires_jsonl_process_without_retrying() {
    let toolset = ClaudeHookToolset::default();
    let mut args = arguments("--persistent");
    args["payload"]["tool_input"]["action"] = json!("hang");
    args["timeout_secs"] = json!(1);
    let error = run(&toolset, args).await.unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert_eq!(
        run(&toolset, arguments("--persistent")).await.unwrap()["result"]["diagnostics"]["count"],
        1
    );
}

async fn wait_for_marker(marker: &Path) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("hook started");
}

#[tokio::test]
async fn cancellation_and_dropped_future_retire_jsonl_process() {
    let directory = tempfile::tempdir().unwrap();
    for dropped in [false, true] {
        let toolset = ClaudeHookToolset::default();
        let marker = directory.path().join(format!("started-{dropped}"));
        let mut args = arguments("--persistent");
        args["payload"]["tool_input"]["action"] = json!("hang");
        args["payload"]["marker"] = json!(marker);
        let cancel = CancellationToken::new();
        {
            let mut running = Box::pin(toolset.invoke("run", args, cancel.clone()));
            tokio::select! {
                result = &mut running => panic!("hook returned before cancellation: {result:?}"),
                () = wait_for_marker(&marker) => {}
            }
            if !dropped {
                cancel.cancel();
                assert!(matches!(running.await, Err(ToolInvokeError::Fatal(_))));
            }
        }
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "1");
        assert_eq!(
            run(&toolset, arguments("--persistent")).await.unwrap()["result"]["diagnostics"]
                ["count"],
            1
        );
    }
}

#[tokio::test]
async fn jsonl_large_request_and_eager_notice_do_not_deadlock() {
    let toolset = ClaudeHookToolset::default();
    let mut args = arguments("--persistent");
    args["command"]
        .as_array_mut()
        .unwrap()
        .push(json!("--eager-notice"));
    args["payload"]["padding"] = json!("x".repeat(128 * 1024));
    let result = run(&toolset, args).await.unwrap();
    assert_eq!(result["decision"], "allow");
}

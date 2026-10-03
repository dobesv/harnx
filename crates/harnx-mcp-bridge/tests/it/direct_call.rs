//! Standalone invocation against the pinned SDK's real stdio server.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    fs::{File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    process::Output,
    time::Duration,
};
use tokio::process::Command;

#[cfg(unix)]
mod descendant_cleanup;

const DEADLINE: Duration = Duration::from_secs(60);

fn sibling_binary(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_harnx-mcp-bridge"))
        .parent()
        .expect("workspace binaries directory")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "build workspace first: {} is missing",
        path.display()
    );
    path
}

fn mixed_result(is_error: bool) -> Value {
    json!({
        "content":[
            {"type":"text","text":"plain text with spaces and \"quotes\""},
            {"type":"image","data":"AA==","mimeType":"image/png"},
            {"type":"audio","data":"AA==","mimeType":"audio/wav"},
            {"type":"resource_link","uri":"file:///fixture-link","name":"fixture","mimeType":"text/plain"},
            {"type":"resource","resource":{"uri":"file:///embedded","mimeType":"text/plain","text":"embedded content"}}
        ],
        "structuredContent":{"nested":{"values":[1,null,{"quote":"\""}]},"ok":!is_error},
        "_meta":{"fixture":"direct-call","nested":{"retained":true}},
        "isError":is_error
    })
}

struct Fixture {
    dir: tempfile::TempDir,
    script: PathBuf,
    spawn_log: PathBuf,
    request_log: PathBuf,
    lifetime_lock: PathBuf,
    gate: PathBuf,
}

impl Fixture {
    fn new(overrides: Value) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let script = dir.path().join("script.yaml");
        let mut body = json!({"tools":[{"name":"mixed_tool","description":"deterministic result"}],"responses":["legacy text"]});
        body.as_object_mut()
            .unwrap()
            .extend(overrides.as_object().unwrap().clone());
        // JSON is YAML; the existing mock loads this with serde_yaml and rmcp types.
        std::fs::write(&script, serde_json::to_vec(&body)?)?;
        Ok(Self {
            script,
            spawn_log: dir.path().join("child.pid"),
            request_log: dir.path().join("requests.jsonl"),
            lifetime_lock: dir.path().join("child.lock"),
            gate: dir.path().join("start.gate"),
            dir,
        })
    }

    fn command(&self, flags: &[&str], gated: bool) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_harnx-mcp-bridge"));
        command
            .args(flags)
            .arg("--")
            .arg(sibling_binary("harnx-mock-mcp"))
            .arg("--script")
            .arg(&self.script)
            .arg("--spawn-log")
            .arg(&self.spawn_log)
            .arg("--request-log")
            .arg(&self.request_log)
            .arg("--lifetime-lock")
            .arg(&self.lifetime_lock)
            .arg("--linger");
        if gated {
            command.arg("--start-gate").arg(&self.gate);
        }
        // Neither a real broker nor inherited worker identity may be needed.
        command
            .env_remove("HARNX_NATS_URL")
            .env_remove("HARNX_NATS_TOKEN")
            .env_remove("HARNX_NATS_SERVER")
            .env_remove("HARNX_SERVER_SCOPE")
            .env("HARNX_LOG_LEVEL", "info")
            .env("HARNX_LOG_FORMAT", "text")
            .env("HARNX_LOG_FILTER", "harnx")
            .env("HARNX_STATE_DIR", self.dir.path())
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        command
    }

    async fn rejected_before_spawn(&self, flags: &[&str], diagnostic: &str) -> Result<()> {
        let output = tokio::time::timeout(DEADLINE, self.command(flags, false).output()).await??;
        assert!(!output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty(), "unexpected stdout: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(diagnostic),
            "{output:?}"
        );
        assert!(
            !self.spawn_log.exists(),
            "invalid command spawned the wrapped child"
        );
        assert!(
            !self.lifetime_lock.exists(),
            "child started before validation"
        );
        Ok(())
    }

    async fn run(&self, flags: &[&str]) -> Result<Output> {
        let mut child = self.command(flags, true).spawn()?;
        let bridge_pid = child.id().context("bridge PID")?;
        let pid = tokio::time::timeout(DEADLINE, async {
            let pid = loop {
                if let Ok(text) = std::fs::read_to_string(&self.spawn_log) {
                    if let Ok(pid) = text.trim().parse::<u32>() {
                        break pid;
                    }
                }
                assert!(
                    child.try_wait()?.is_none(),
                    "bridge exited before mock recorded PID"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            Ok::<_, anyhow::Error>(pid)
        })
        .await??;
        assert_ne!(
            pid, bridge_pid,
            "recorded bridge rather than wrapped child PID"
        );
        assert!(pid > 0);
        let lock = open_lock(&self.lifetime_lock)?;
        assert!(
            matches!(lock.try_lock(), Err(TryLockError::WouldBlock)),
            "child {pid} did not hold its lifetime lock"
        );
        std::fs::write(&self.gate, "start")?;
        let output = tokio::time::timeout(DEADLINE, child.wait_with_output()).await??;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("child pid {pid}")),
            "spawn audit did not identify bridge's actual wrapped PID {pid}: {stderr}"
        );
        // This mock ignores Unix parent-death SIGTERM and stays alive after stdio
        // closure. Only termination releases this lock; EOF alone cannot pass.
        lock.try_lock()
            .with_context(|| format!("wrapped child {pid} survived bridge exit"))?;
        lock.unlock()?;
        Ok(output)
    }

    fn requests(&self) -> Result<Vec<Value>> {
        std::fs::read_to_string(&self.request_log)?
            .lines()
            .map(|line| serde_json::from_str(line).map_err(Into::into))
            .collect()
    }

    fn assert_initialized_call(&self, args: &Value) -> Result<()> {
        let events = self.requests()?;
        let mut methods: Vec<_> = events
            .iter()
            .map(|event| event["method"].as_str().unwrap())
            .collect();
        assert_eq!(events[0]["method"], "initialize");
        methods.sort_unstable();
        assert_eq!(
            methods,
            [
                "initialize",
                "notifications/initialized",
                "tools/call",
                "tools/list"
            ]
        );
        let listed = events
            .iter()
            .position(|event| event["method"] == "tools/list")
            .unwrap();
        let called = events
            .iter()
            .position(|event| event["method"] == "tools/call")
            .unwrap();
        assert!(listed < called, "call happened before tools/list");
        assert!(events[0]["params"]["protocolVersion"].as_str().is_some());
        assert_eq!(events[called]["params"]["name"], "mixed_tool");
        assert_eq!(&events[called]["params"]["arguments"], args);
        Ok(())
    }
}

fn open_lock(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(Into::into)
}

fn assert_json_result(output: &Output, expected: &Value, success: bool) -> Result<()> {
    assert_eq!(
        output.status.success(),
        success,
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value =
        serde_json::from_slice(&output.stdout).context("stdout is one complete JSON result")?;
    assert_eq!(
        &value, expected,
        "full MCP content/schema/metadata fidelity"
    );
    Ok(())
}

#[tokio::test]
async fn mixed_structured_meta_success_preserved_without_nats_and_child_reaped() -> Result<()> {
    let expected = mixed_result(false);
    let fixture = Fixture::new(json!({"call_result":expected}))?;
    let args = json!({"text":"spaces and \"quotes\"","nested":{"items":[1,2]},"optional":null});
    let raw = args.to_string();
    let output = fixture
        .run(&["--call-tool", "mixed_tool", "--tool-args", &raw])
        .await?;
    assert_json_result(&output, &expected, true)?;
    assert!(!String::from_utf8_lossy(&output.stderr).contains("error:"));
    fixture.assert_initialized_call(&args)?;
    Ok(())
}

#[tokio::test]
async fn mixed_structured_meta_tool_error_is_json_nonzero_with_diagnostic_and_child_reaped(
) -> Result<()> {
    let expected = mixed_result(true);
    let fixture = Fixture::new(json!({"call_result":expected}))?;
    let output = fixture
        .run(&[
            "--name",
            "explicit",
            "--call-tool",
            "mixed_tool",
            "--tool-args",
            "{}",
        ])
        .await?;
    assert_json_result(&output, &expected, false)?;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("reported isError: true"));
    fixture.assert_initialized_call(&json!({}))?;
    Ok(())
}

#[tokio::test]
async fn omitted_tool_arguments_default_to_object() -> Result<()> {
    let expected = mixed_result(false);
    let fixture = Fixture::new(json!({"call_result":expected}))?;
    let output = fixture.run(&["--call-tool", "mixed_tool"]).await?;
    assert_json_result(&output, &expected, true)?;
    fixture.assert_initialized_call(&json!({}))?;
    Ok(())
}

#[tokio::test]
async fn initialized_tools_call_json_rpc_error_is_not_tool_result_or_startup_failure() -> Result<()>
{
    let fixture = Fixture::new(
        json!({"call_error":{"code":-32602,"message":"invocation protocol sentinel","data":{"reason":"deterministic invalid params"}}}),
    )?;
    let output = fixture.run(&["--call-tool", "mixed_tool"]).await?;
    fixture.assert_initialized_call(&json!({}))?;
    assert_eq!(output.status.code(), Some(1));
    assert!(
        output.stdout.is_empty(),
        "protocol error fabricated a CallToolResult"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("tool invocation failed"), "{stderr}");
    assert!(
        stderr.contains("-32602") && stderr.contains("invocation protocol sentinel"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("failed to connect to MCP child server"),
        "{stderr}"
    );
    Ok(())
}

#[tokio::test]
async fn initialized_transport_disconnect_has_distinct_failure_and_child_reaped() -> Result<()> {
    let fixture = Fixture::new(json!({"disconnect_on_call":true}))?;
    let output = fixture.run(&["--call-tool", "mixed_tool"]).await?;
    fixture.assert_initialized_call(&json!({}))?;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("tool invocation failed"), "{stderr}");
    assert!(!stderr.contains("invocation protocol sentinel"));
    Ok(())
}

#[tokio::test]
async fn startup_protocol_error_reaps_lingering_child_without_invoking() -> Result<()> {
    let fixture =
        Fixture::new(json!({"initialize_error":{"code":-32602,"message":"initialize sentinel"}}))?;
    let output = fixture.run(&["--call-tool", "mixed_tool"]).await?;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("failed to connect to MCP child server")
    );
    let events = fixture.requests()?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["method"], "initialize");
    Ok(())
}

#[tokio::test]
async fn unknown_tool_after_discovery_fails_without_call_and_reaps_child() -> Result<()> {
    let fixture = Fixture::new(json!({}))?;
    let output = fixture.run(&["--call-tool", "unknown_tool"]).await?;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("tool 'unknown_tool' not found"));
    // Assert initialize came first (causality requirement), but allow notifications/list
    // in any order since they're dispatched concurrently from stdio callbacks.
    // The critical invariants: initialize first, no tools/call (tool not found).
    let events = fixture.requests()?;
    let mut methods: Vec<_> = events
        .iter()
        .map(|event| event["method"].as_str().unwrap())
        .collect();
    assert_eq!(
        methods.first(),
        Some(&"initialize"),
        "initialize must be first"
    );
    methods.sort_unstable();
    assert_eq!(
        methods,
        ["initialize", "notifications/initialized", "tools/list"],
        "no tools/call should be present (unknown tool)"
    );
    // Verify no tools/call was attempted (key evidence of early failure)
    assert!(
        !events.iter().any(|e| e["method"] == "tools/call"),
        "must not attempt tool call for unknown tool"
    );
    Ok(())
}

#[tokio::test]
async fn malformed_and_nonobject_json_are_rejected_before_spawn() -> Result<()> {
    for (raw, diagnostic) in [
        ("{bad", "invalid JSON in --tool-args"),
        ("", "invalid JSON in --tool-args"),
        ("null", "must be a JSON object"),
        ("[]", "must be a JSON object"),
        ("1", "must be a JSON object"),
        ("true", "must be a JSON object"),
        ("\"text\"", "must be a JSON object"),
    ] {
        Fixture::new(json!({}))?
            .rejected_before_spawn(
                &["--call-tool", "mixed_tool", "--tool-args", raw],
                diagnostic,
            )
            .await?;
    }
    Ok(())
}

#[tokio::test]
async fn incompatible_modes_and_arguments_require_call_even_for_default_object() -> Result<()> {
    for flags in [
        vec!["--tool-args", "{}"],
        vec!["--tool-args={}"],
        vec!["--tool-args", ""],
        vec!["--list-tools", "--tool-args", "{}"],
        vec!["--name", "named", "--tool-args", "{}"],
        vec!["--list-tools", "--call-tool", "mixed_tool"],
    ] {
        Fixture::new(json!({}))?
            .rejected_before_spawn(&flags, "--call-tool")
            .await?;
    }
    Ok(())
}

#[tokio::test]
async fn enable_tool_filters_apply_to_direct_calls_before_child_spawn() -> Result<()> {
    Fixture::new(json!({}))?
        .rejected_before_spawn(
            &["--call-tool", "mixed_tool", "--enable-tool", "other*"],
            "excluded by --enable-tool",
        )
        .await?;
    Fixture::new(json!({}))?
        .rejected_before_spawn(&["--call-tool", "mixed_tool", "--enable-tool", "["], "glob")
        .await?;
    let expected = mixed_result(false);
    let fixture = Fixture::new(json!({"call_result":expected}))?;
    let output = fixture
        .run(&["--call-tool", "mixed_tool", "--enable-tool", "mixed_*"])
        .await?;
    assert_json_result(&output, &expected, true)?;
    Ok(())
}

#[tokio::test]
async fn existing_list_mode_keeps_human_output_filters_and_child_cleanup() -> Result<()> {
    let fixture = Fixture::new(json!({}))?;
    let output = fixture.run(&["--list-tools", "--name", "fixture"]).await?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.starts_with("MCP server 'fixture': 1 tool(s)\n"),
        "{stdout}"
    );
    assert!(stdout.contains("mixed_tool"));
    let fixture = Fixture::new(json!({}))?;
    let filtered = fixture
        .run(&["--list-tools", "--enable-tool", "other*"])
        .await?;
    assert!(filtered.status.success());
    assert_eq!(
        String::from_utf8(filtered.stdout)?,
        "MCP server 'mcp-diagnostic': 0 tool(s)\n\n  (the server completed its handshake but advertises no tools)\n"
    );
    assert_eq!(fixture.requests()?.len(), 3);
    Ok(())
}

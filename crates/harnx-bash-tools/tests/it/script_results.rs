//! `exec` and command templates answer a script like `bash -c`: stdout,
//! stderr and the exit status as separate fields, while a call without the
//! structured-result capability keeps its model-facing text.

use harnx_bash_tools::{discover_tool_templates, BashToolset};
use harnx_sandbox_common::SandboxConfig;
use harnx_tool_allow::ResolvedAllowlist;
use harnx_toolset::{ToolInvocation, ToolInvocationContext, Toolset, CAPABILITY_STRUCTURED_RESULT};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn sandbox_config(root: &std::path::Path) -> SandboxConfig {
    let mut allowlist = ResolvedAllowlist::new();
    allowlist.insert_rwx(root);
    SandboxConfig {
        enabled: false,
        allowlist: Arc::new(allowlist),
        extra_env_passthrough: vec![],
        env_overrides: vec![],
        sandbox_run_path: PathBuf::from("harnx-sandbox-exec"),
    }
}

pub(crate) async fn toolset_in(root: &std::path::Path) -> BashToolset {
    BashToolset::new(sandbox_config(root), Vec::new())
        .await
        .expect("build toolset")
}

pub(crate) async fn call(toolset: &BashToolset, tool: &str, args: Value, script: bool) -> Value {
    let mut context = ToolInvocationContext::default();
    if script {
        context
            .capabilities
            .insert(CAPABILITY_STRUCTURED_RESULT.to_string());
    }
    toolset
        .invoke_with_context(ToolInvocation {
            tool: tool.to_string(),
            args,
            context,
            cancel: CancellationToken::new(),
        })
        .await
        .expect("tool result")
}

async fn script_exec(command: &str, extra: Value) -> Value {
    let directory = tempfile::tempdir().expect("temp dir");
    let toolset = toolset_in(directory.path()).await;
    let mut args = json!({ "command": command, "working_dir": directory.path() });
    args.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    call(&toolset, "exec", args, true).await
}

#[tokio::test]
async fn exec_false_reports_exit_code_1() {
    let result = script_exec("false", json!({})).await;
    assert_eq!(result["structuredContent"]["exit_code"], json!(1));
    assert_eq!(result["structuredContent"]["stdout"], json!(""));
    assert_ne!(result["isError"], json!(true));
}

#[tokio::test]
async fn exec_separates_stdout_and_stderr() {
    let result = script_exec("echo out; echo err >&2", json!({})).await;
    let fields = &result["structuredContent"];
    assert_eq!(fields["stdout"], json!("out\n"));
    assert_eq!(fields["stderr"], json!("err\n"));
    assert_eq!(fields["exit_code"], json!(0));
    assert_eq!(fields["status"], json!("exited"));
}

#[tokio::test]
async fn exec_keeps_bytes_as_printed() {
    let result = script_exec("printf x", json!({})).await;
    assert_eq!(result["structuredContent"]["stdout"], json!("x"));
}

#[tokio::test]
async fn exec_timeout_reports_124_and_is_not_an_error() {
    let result = script_exec("printf partial; sleep 30", json!({ "timeout_secs": 1 })).await;
    let fields = &result["structuredContent"];
    assert_ne!(result["isError"], json!(true), "{result}");
    assert_eq!(fields["status"], json!("timeout"));
    assert_eq!(fields["exit_code"], json!(124));
    assert_eq!(fields["stdout"], json!("partial"));
    let notes = fields["notes"].as_array().expect("notes");
    assert!(
        notes
            .iter()
            .any(|n| n == "timed out after 1s and was terminated"),
        "{notes:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn exec_killed_by_signal_reports_137() {
    let result = script_exec("kill -9 $$", json!({})).await;
    let fields = &result["structuredContent"];
    assert_eq!(fields["exit_code"], json!(137));
    assert_eq!(fields["status"], json!("exited"));
    assert!(fields["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n == "ended by signal 9"));
}

#[tokio::test]
async fn direct_exec_result_is_unchanged() {
    let directory = tempfile::tempdir().expect("temp dir");
    let toolset = toolset_in(directory.path()).await;
    for (command, timeout) in [("true", 60), ("false", 60), ("sleep 30", 1)] {
        let args = json!({
            "command": command,
            "working_dir": directory.path(),
            "timeout_secs": timeout,
        });
        let result = call(&toolset, "exec", args, false).await;
        assert!(result.get("structuredContent").is_none(), "{result}");
        let text = result["content"][0]["text"].as_str().expect("text block");
        let timed_out = command == "sleep 30";
        let expected_start = if timed_out {
            "command timed out"
        } else {
            "```yaml\n"
        };
        assert!(text.starts_with(expected_start), "{text}");
        assert_eq!(result["isError"] == json!(true), timed_out);
    }
}

#[tokio::test]
async fn plain_template_gets_the_exec_mapping_and_jsonl_template_does_not() {
    let directory = tempfile::tempdir().expect("temp dir");
    gix::init(directory.path()).expect("git init");
    let write = |name: &str, extra: &str, script: &str| {
        let path = directory.path().join(format!("{name}.yaml"));
        std::fs::write(
            &path,
            format!(
                "name: {name}\ndescription: Test {name}\n{extra}sandbox:\n  enabled: false\nscript: |\n  {script}\n"
            ),
        )
        .expect("write template");
        path
    };
    let paths = vec![
        write("plain", "", "echo hi"),
        write("streaming", "protocol: jsonl\n", "echo hi"),
        write("writer", "", "echo $$ > note.txt"),
    ];
    let templates = discover_tool_templates(None, &paths, &[]).expect("discover templates");
    let toolset = BashToolset::new(sandbox_config(directory.path()), templates)
        .await
        .expect("build toolset");
    let specs = toolset.tools();
    let output = |name: &str| {
        specs
            .iter()
            .find(|spec| spec.name == name)
            .unwrap_or_else(|| panic!("{name} registered"))
            .script_output()
    };
    assert_eq!(
        output("plain").expect("plain mapping").stdout.as_deref(),
        Some("stdout")
    );
    assert!(output("streaming").is_none());
    assert!(output("exec").is_some());
    let schema = |name: &str| {
        specs
            .iter()
            .find(|spec| spec.name == name)
            .and_then(|spec| spec.output_schema().cloned())
    };
    assert!(schema("exec").is_some());
    assert_eq!(schema("plain"), schema("exec"));
    assert!(schema("streaming").is_none());

    let result = call(&toolset, "plain", json!({}), true).await;
    assert_eq!(result["structuredContent"]["stdout"], json!("hi\n"));
    assert!(
        result["structuredContent"].get("diff").is_none(),
        "{result}"
    );

    let result = call(&toolset, "writer", json!({}), true).await;
    let diff = result["structuredContent"]["diff"].as_str().expect("diff");
    assert!(diff.contains(": git -C "), "{result}");
    let direct = call(&toolset, "writer", json!({}), false).await;
    assert!(direct.to_string().contains("```diff"), "{direct}");
}

/// A toolset in its own working directory, with the helpers the spawn tests
/// share. Each test ends the processes it started through `terminate`.
struct Spawner {
    directory: tempfile::TempDir,
    toolset: BashToolset,
}

impl Spawner {
    async fn new() -> Self {
        let directory = tempfile::tempdir().expect("temp dir");
        let toolset = toolset_in(directory.path()).await;
        Self { directory, toolset }
    }

    async fn call(&self, tool: &str, args: Value, script: bool) -> Value {
        call(&self.toolset, tool, args, script).await
    }

    async fn spawn(&self, command: &str, script: bool) -> Value {
        let args = json!({ "command": command, "working_dir": self.directory.path() });
        self.call("spawn", args, script).await
    }

    async fn spawn_id(&self, command: &str) -> String {
        let result = self.spawn(command, true).await;
        result["structuredContent"]["execution_id"]
            .as_str()
            .expect("execution id")
            .to_string()
    }

    async fn terminate(&self, execution_id: &str) {
        let args = json!({ "execution_id": execution_id, "signal": "SIGKILL" });
        let _ = self.call("terminate", args, false).await;
    }
}

fn has_note(fields: &Value, note: &str) -> bool {
    fields["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .any(|n| n == note)
}

#[tokio::test]
async fn spawn_without_wait_prints_status_json() {
    let spawner = Spawner::new().await;
    let result = spawner.spawn("sleep 30", true).await;
    let fields = result["structuredContent"].clone();
    let id = fields["execution_id"].as_str().expect("id").to_string();
    spawner.terminate(&id).await;
    assert!(fields.get("stdout").is_none(), "{fields}");
    assert_eq!(fields["status"], json!("spawned"));
    assert_eq!(fields["notes"], json!([]));
    assert!(fields["stdout_log_path"].is_string());
}

#[tokio::test]
async fn wait_on_a_finished_command_reports_like_exec() {
    let spawner = Spawner::new().await;
    let id = spawner
        .spawn_id("sh -c 'echo out; echo err >&2; exit 3'")
        .await;
    let args = json!({ "execution_id": id, "seconds": 20 });
    let result = spawner.call("wait", args, true).await;
    let fields = &result["structuredContent"];
    assert_eq!(fields["exit_code"], json!(3), "{result}");
    assert_eq!(fields["stdout"], json!("out\n"));
    assert_eq!(fields["stderr"], json!("err\n"));
    assert_eq!(fields["status"], json!("exited"));
    let status = spawner
        .call("status", json!({ "execution_id": id }), true)
        .await;
    assert_eq!(status["structuredContent"]["exit_code"], json!(3));
}

#[tokio::test]
async fn wait_reads_a_log_that_is_not_utf8_lossily() {
    let spawner = Spawner::new().await;
    let id = spawner.spawn_id("printf '\\377x\\n'").await;
    let args = json!({ "execution_id": id, "seconds": 20 });
    let result = spawner.call("wait", args, true).await;
    assert_eq!(
        result["structuredContent"]["stdout"],
        json!("\u{fffd}x\n"),
        "{result}"
    );
}

#[tokio::test]
async fn wait_that_ends_before_the_command_reports_running() {
    let spawner = Spawner::new().await;
    let id = spawner.spawn_id("sleep 30").await;
    let args = json!({ "execution_id": id, "seconds": 1 });
    let result = spawner.call("wait", args, true).await;
    spawner.terminate(&id).await;
    let pending_names = declared_properties("wait", &spawner.toolset);
    assert_declared("wait", &pending_names, &result);
    let fields = &result["structuredContent"];
    assert_eq!(fields["status"], json!("running"), "{result}");
    assert!(fields.get("stdout").is_none());
    assert!(has_note(
        fields,
        "still running; stdout is its status as JSON"
    ));
}

#[tokio::test]
async fn status_on_a_running_command_reports_running() {
    let spawner = Spawner::new().await;
    let id = spawner.spawn_id("sleep 30").await;
    let result = spawner
        .call("status", json!({ "execution_id": id }), true)
        .await;
    spawner.terminate(&id).await;
    let pending_names = declared_properties("wait", &spawner.toolset);
    assert_declared("wait", &pending_names, &result);
    let fields = &result["structuredContent"];
    assert_eq!(fields["status"], json!("running"), "{result}");
    assert_eq!(fields["execution_id"], json!(id));
    assert!(fields["running_for_secs"].is_u64());
}

#[cfg(unix)]
#[tokio::test]
async fn wait_after_terminate_reports_a_non_zero_status() {
    let spawner = Spawner::new().await;
    let id = spawner.spawn_id("sleep 30").await;
    let args = json!({ "execution_id": id, "signal": "SIGTERM" });
    spawner.call("terminate", args, false).await;
    let args = json!({ "execution_id": id, "seconds": 20 });
    let result = spawner.call("wait", args, true).await;
    let fields = &result["structuredContent"];
    assert_eq!(fields["status"], json!("exited"), "{result}");
    let code = fields["exit_code"].as_i64().expect("exit code");
    assert_eq!(code, 143, "{result}");
}

#[tokio::test]
async fn read_exec_log_prints_plain_lines_for_a_script() {
    let spawner = Spawner::new().await;
    let id = spawner.spawn_id("printf 'a\\nb\\nc\\n'").await;
    spawner
        .call("wait", json!({ "execution_id": id, "seconds": 20 }), true)
        .await;
    let args = json!({ "execution_id": id, "stream": "stdout" });
    let result = spawner.call("read_exec_log", args, true).await;
    let fields = &result["structuredContent"];
    assert_eq!(fields["text"], json!("a\nb\nc\n"), "{result}");
    assert_eq!(fields["start_line"], json!(1));
    assert_eq!(fields["end_line"], json!(3));
    assert_eq!(fields["total_lines"], json!(3));
    assert_eq!(fields["stream"], json!("stdout"));
    assert_eq!(fields["notes"], json!([]));
}

#[tokio::test]
async fn read_exec_log_line_numbers_default_on_for_a_direct_call() {
    let spawner = Spawner::new().await;
    let id = spawner.spawn_id("printf 'a\\nb\\n'").await;
    spawner
        .call("wait", json!({ "execution_id": id, "seconds": 20 }), true)
        .await;
    let base = json!({ "execution_id": id, "stream": "stdout" });
    let direct = spawner.call("read_exec_log", base.clone(), false).await;
    assert!(direct.get("structuredContent").is_none());
    assert_eq!(
        direct["content"][0]["text"],
        json!("1: a\n2: b"),
        "{direct}"
    );

    let mut off = base.clone();
    off["line_numbers"] = json!(false);
    let direct_off = spawner.call("read_exec_log", off, false).await;
    assert_eq!(direct_off["content"][0]["text"], json!("a\nb"));

    let mut on = base;
    on["line_numbers"] = json!(true);
    let script_on = spawner.call("read_exec_log", on, true).await;
    assert_eq!(
        script_on["structuredContent"]["text"],
        json!("1: a\n2: b\n")
    );
}

#[tokio::test]
async fn read_exec_log_paging_note_is_in_notes() {
    let spawner = Spawner::new().await;
    let id = spawner.spawn_id("seq 1 10").await;
    spawner
        .call("wait", json!({ "execution_id": id, "seconds": 20 }), true)
        .await;
    let args = json!({ "execution_id": id, "stream": "stdout", "offset": 1, "limit": 3 });
    let result = spawner.call("read_exec_log", args, true).await;
    let fields = &result["structuredContent"];
    assert_eq!(fields["text"], json!("1\n2\n3\n"), "{result}");
    assert!(
        has_note(fields, "7 more lines. Use --offset 4 to continue"),
        "{fields}"
    );
    assert_eq!(fields["total_lines"], json!(10));
}

async fn log_of(spawner: &Spawner, command: &str) -> String {
    let id = spawner.spawn_id(command).await;
    spawner
        .call("wait", json!({ "execution_id": id, "seconds": 20 }), true)
        .await;
    id
}

async fn read_log(spawner: &Spawner, id: &str, extra: Value) -> Value {
    let mut args = json!({ "execution_id": id, "stream": "stdout" });
    args.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    spawner.call("read_exec_log", args, true).await["structuredContent"].clone()
}

fn first_line(fields: &Value) -> String {
    fields["text"]
        .as_str()
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn read_exec_log_without_offset_notes_the_default_limit() {
    let spawner = Spawner::new().await;
    let id = log_of(&spawner, "seq 1 300").await;
    let fields = read_log(&spawner, &id, json!({})).await;
    assert_eq!(fields["end_line"], json!(200), "{fields}");
    assert_eq!(
        fields["notes"],
        json!(["100 more lines. Use --offset 201 to continue"])
    );
    let next = read_log(&spawner, &id, json!({ "offset": 201 })).await;
    assert_eq!(first_line(&next), "201");
    assert_eq!(next["end_line"], json!(300));
    assert_eq!(next["notes"], json!([]));
}

#[tokio::test]
async fn read_exec_log_byte_cap_names_the_first_dropped_line() {
    let spawner = Spawner::new().await;
    let id = log_of(&spawner, "seq 1 20").await;
    // "1\n" .. "9\n" are 18 bytes and "10\n" brings it to 21.
    let fields = read_log(
        &spawner,
        &id,
        json!({ "offset": 1, "max_output_bytes": 20 }),
    )
    .await;
    assert_eq!(fields["end_line"], json!(9), "{fields}");
    assert!(
        has_note(&fields, "11 more lines. Use --offset 10 to continue"),
        "{fields}"
    );
    let next = read_log(&spawner, &id, json!({ "offset": 10, "limit": 2 })).await;
    assert_eq!(next["text"], json!("10\n11\n"));
}

#[tokio::test]
async fn read_exec_log_tail_has_no_continuation_note() {
    let spawner = Spawner::new().await;
    let id = log_of(&spawner, "seq 1 300").await;
    let fields = read_log(&spawner, &id, json!({ "tail": 5 })).await;
    assert_eq!(fields["text"], json!("296\n297\n298\n299\n300\n"));
    assert!(
        fields["notes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|note| !note.as_str().unwrap().contains("--offset")),
        "{fields}"
    );
}

#[tokio::test]
async fn read_exec_log_of_an_empty_log_with_an_offset_is_empty() {
    let spawner = Spawner::new().await;
    let id = log_of(&spawner, "true").await;
    let fields = read_log(&spawner, &id, json!({ "offset": 5 })).await;
    assert_eq!(fields["text"], json!(""), "{fields}");
    assert_eq!(fields["total_lines"], json!(0));
}

#[tokio::test]
async fn direct_wait_and_spawn_results_are_unchanged() {
    let spawner = Spawner::new().await;
    let spawned = spawner.spawn("sleep 30", false).await;
    assert!(spawned.get("structuredContent").is_none(), "{spawned}");
    let text = spawned["content"][0]["text"].as_str().expect("text");
    assert!(text.starts_with("```yaml\n"), "{text}");
    assert!(text.contains("status: spawned"), "{text}");
    let id = text
        .lines()
        .find_map(|line| line.strip_prefix("execution_id: "))
        .expect("execution id line")
        .trim_matches('\'')
        .to_string();
    let args = json!({ "execution_id": id, "seconds": 1 });
    let waited = spawner.call("wait", args, false).await;
    spawner.terminate(&id).await;
    assert!(waited.get("structuredContent").is_none(), "{waited}");
    let text = waited["content"][0]["text"].as_str().expect("text");
    assert!(text.contains("status: running"), "{text}");
}

#[tokio::test]
async fn exec_cut_note_names_the_log_command_and_where_to_continue() {
    let result = script_exec("seq 1 100", json!({ "max_output_bytes": 20 })).await;
    let fields = &result["structuredContent"];
    let id = fields["execution_id"].as_str().expect("id");
    assert_eq!(fields["stdout"].as_str().unwrap().len(), 18);
    let expected = format!(
        "stdout cut at 18 of 292 bytes; full log: bash_read_exec_log --execution-id {id} --stream stdout --offset 10"
    );
    assert!(has_note(fields, &expected), "{fields}");
}

#[tokio::test]
async fn exec_cut_in_the_middle_of_a_line_names_no_offset() {
    let result = script_exec("printf 'abcdefghij\\n'", json!({ "max_output_bytes": 4 })).await;
    let notes = result["structuredContent"]["notes"].to_string();
    assert!(notes.contains("stdout cut at 4 of 11 bytes"), "{notes}");
    assert!(!notes.contains("--offset"), "{notes}");
}

#[tokio::test]
async fn line_caps_are_ignored_in_a_script_with_a_note() {
    let spawner = Spawner::new().await;
    let args = json!({
        "command": "echo hi",
        "working_dir": spawner.directory.path(),
        "tail_lines": 1,
        "head_lines": 1,
    });
    let result = spawner.call("exec", args, true).await;
    let fields = &result["structuredContent"];
    assert!(
        has_note(fields, "tail_lines is ignored in a script; pipe to tail"),
        "{fields}"
    );
    assert!(
        has_note(fields, "head_lines is ignored in a script; pipe to head"),
        "{fields}"
    );

    let id = spawner.spawn_id("echo hi").await;
    spawner
        .call("wait", json!({ "execution_id": id, "seconds": 20 }), true)
        .await;
    let waited = spawner
        .call("wait", json!({ "execution_id": id, "tail_lines": 1 }), true)
        .await;
    let fields = &waited["structuredContent"];
    assert!(
        has_note(fields, "tail_lines is ignored in a script; pipe to tail"),
        "{fields}"
    );
    let status = spawner
        .call(
            "status",
            json!({ "execution_id": id, "head_lines": 1 }),
            true,
        )
        .await;
    let fields = &status["structuredContent"];
    assert!(
        has_note(fields, "head_lines is ignored in a script; pipe to head"),
        "{fields}"
    );

    let running = spawner.spawn_id("sleep 30").await;
    let status = spawner
        .call(
            "status",
            json!({ "execution_id": running, "tail_lines": 1 }),
            true,
        )
        .await;
    spawner.terminate(&running).await;
    let fields = &status["structuredContent"];
    assert_eq!(fields["status"], json!("running"));
    assert!(
        has_note(fields, "tail_lines is ignored in a script; pipe to tail"),
        "{fields}"
    );
}

#[tokio::test]
async fn spawn_help_says_an_unfinished_command_prints_status_json() {
    let toolset = toolset_in(tempfile::tempdir().unwrap().path()).await;
    let specs = toolset.tools();
    for name in ["spawn", "wait", "status"] {
        let summary = specs
            .iter()
            .find(|spec| spec.name == name)
            .and_then(|spec| spec.script_output())
            .and_then(|output| output.summary)
            .expect("summary");
        assert_eq!(
            summary,
            "a finished command's stdout, stderr and exit status; otherwise one line of status JSON"
        );
    }
}

#[tokio::test]
async fn spawned_command_past_its_own_deadline_reports_a_timeout() {
    let spawner = Spawner::new().await;
    let args = json!({
        "command": "sleep 30",
        "working_dir": spawner.directory.path(),
        "timeout_secs": 1,
    });
    let spawned = spawner.call("spawn", args, true).await;
    let id = spawned["structuredContent"]["execution_id"]
        .as_str()
        .expect("id")
        .to_string();
    let args = json!({ "execution_id": id, "seconds": 20 });
    let result = spawner.call("wait", args, true).await;
    let fields = &result["structuredContent"];
    assert_eq!(fields["status"], json!("timeout"), "{result}");
    assert_eq!(fields["exit_code"], json!(124));
    assert!(has_note(fields, "timed out after 1s and was terminated"));
}

/// A git repository holding one tracked-looking file, with the toolset that
/// may work in it.
struct Repo {
    directory: tempfile::TempDir,
    toolset: BashToolset,
}

impl Repo {
    async fn new() -> Self {
        let directory = tempfile::tempdir().expect("temp dir");
        gix::init(directory.path()).expect("git init");
        std::fs::write(directory.path().join("note.txt"), "before\n").expect("write note");
        let toolset = toolset_in(directory.path()).await;
        Self { directory, toolset }
    }

    /// An `exec` call that rewrites the note, with `extra` arguments merged in.
    fn write_note(&self, text: &str, extra: Value) -> Value {
        let mut args = json!({
            "command": format!("printf '{text}\\n' > note.txt"),
            "working_dir": self.directory.path(),
        });
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        args
    }

    /// The note-rewriting `exec` call, made as a script or as a model.
    async fn exec_note(&self, text: &str, extra: Value, script: bool) -> Value {
        call(&self.toolset, "exec", self.write_note(text, extra), script).await
    }

    /// Spawns the note-rewriting command as a script and returns its id.
    async fn spawn_note(&self, extra: Value) -> String {
        let args = self.write_note("after", extra);
        let spawned = call(&self.toolset, "spawn", args, true).await;
        spawned["structuredContent"]["execution_id"]
            .as_str()
            .expect("id")
            .to_string()
    }

    fn object_count(&self) -> u64 {
        let output = std::process::Command::new("git")
            .args(["count-objects", "-v"])
            .current_dir(self.directory.path())
            .output()
            .expect("git count-objects");
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.lines()
            .filter_map(|line| line.strip_prefix("count: "))
            .next()
            .expect("count line")
            .trim()
            .parse()
            .expect("count")
    }
}

/// The `diff` field of a script-style result.
pub(crate) fn diff_field(result: &Value) -> &str {
    result["structuredContent"]["diff"]
        .as_str()
        .unwrap_or_else(|| panic!("no diff field: {result}"))
}

/// The text blocks of a direct result.
fn texts(result: &Value) -> Vec<String> {
    result["content"]
        .as_array()
        .expect("content")
        .iter()
        .filter_map(|block| block["text"].as_str().map(str::to_string))
        .collect()
}

/// The before and after ids on a summary line.
pub(crate) fn summary_ids(line: &str) -> (String, String) {
    let mut words = line.split_whitespace().rev();
    let after = words.next().expect("after id");
    let before = words.next().expect("before id");
    assert_eq!(before.len(), 12, "{line}");
    assert_eq!(after.len(), 12, "{line}");
    assert!(after.chars().all(|c| c.is_ascii_hexdigit()), "{line}");
    (before.to_string(), after.to_string())
}

/// A script's diff is a summary line by default, and the command's own
/// output is left alone.
#[tokio::test]
async fn exec_in_a_script_reports_a_summary_by_default() {
    let repo = Repo::new().await;
    let script = repo.exec_note("after", json!({}), true).await;
    let fields = &script["structuredContent"];
    assert_eq!(fields["exit_code"], json!(0), "{script}");
    assert_eq!(fields["stdout"], json!(""), "{script}");
    let diff = diff_field(&script);
    assert_eq!(diff.lines().count(), 1, "{diff}");
    assert!(diff.ends_with('\n'), "{diff:?}");
    assert!(diff.starts_with("1 file changed"), "{diff}");
    assert!(diff.contains(": git -C "), "{diff}");
    summary_ids(diff.trim_end());
    assert!(!script.to_string().contains("```diff"), "{script}");
    let names = declared_properties("exec", &repo.toolset);
    assert_declared("exec", &names, &script);
}

#[tokio::test]
async fn exec_in_a_script_reports_each_level() {
    let repo = Repo::new().await;
    let files = repo
        .exec_note("one", json!({ "diff": "files" }), true)
        .await;
    let diff = diff_field(&files);
    let mut lines = diff.lines();
    assert!(lines.next().unwrap().contains(": git -C "), "{diff}");
    let rest: Vec<&str> = lines.collect();
    assert!(rest.iter().any(|line| line.contains("note.txt")), "{diff}");
    assert!(!diff.contains("```"), "{diff}");

    let full = repo.exec_note("two", json!({ "diff": "full" }), true).await;
    let diff = diff_field(&full);
    assert!(diff.starts_with("commit "), "{diff}");
    assert!(diff.contains("```diff"), "{diff}");

    let none = repo
        .exec_note("three", json!({ "diff": "none" }), true)
        .await;
    assert!(none["structuredContent"].get("diff").is_none(), "{none}");
}

#[tokio::test]
async fn exec_at_none_takes_no_snapshot() {
    let repo = Repo::new().await;
    let before = repo.object_count();
    let result = repo
        .exec_note("after", json!({ "diff": "none" }), false)
        .await;
    assert_eq!(repo.object_count(), before, "{result}");
    assert_eq!(texts(&result).len(), 2, "{result}");
    repo.exec_note("again", json!({ "diff": "summary" }), false)
        .await;
    assert!(repo.object_count() > before);
}

#[tokio::test]
async fn a_direct_exec_reports_the_level_it_was_asked_for() {
    let repo = Repo::new().await;
    let default = repo.exec_note("a", json!({}), false).await;
    let blocks = texts(&default);
    assert_eq!(blocks.len(), 3, "{default}");
    assert!(blocks[2].starts_with("commit "), "{}", blocks[2]);
    assert!(blocks[2].contains("```diff"), "{}", blocks[2]);

    let summary = texts(
        &repo
            .exec_note("b", json!({ "diff": "summary" }), false)
            .await,
    );
    assert_eq!(summary.len(), 3, "{summary:?}");
    assert!(summary[2].contains(": git -C "), "{summary:?}");
    assert!(!summary[2].contains("```"), "{summary:?}");

    let files = texts(&repo.exec_note("c", json!({ "diff": "files" }), false).await);
    assert_eq!(files.len(), 3, "{files:?}");
    assert!(files[2].contains("note.txt"), "{files:?}");

    let none = repo.exec_note("d", json!({ "diff": "none" }), false).await;
    assert_eq!(texts(&none).len(), 2);
}

#[tokio::test]
async fn an_unknown_diff_level_names_the_four() {
    let repo = Repo::new().await;
    let args = repo.write_note("a", json!({ "diff": "loud" }));
    let error = repo
        .toolset
        .invoke_with_context(ToolInvocation {
            tool: "exec".to_string(),
            args,
            context: ToolInvocationContext::default(),
            cancel: CancellationToken::new(),
        })
        .await
        .expect_err("invalid level");
    let message = format!("{error:?}");
    for name in ["none", "summary", "files", "full"] {
        assert!(message.contains(name), "{message}");
    }
}

#[tokio::test]
async fn a_spawned_command_is_reported_at_each_callers_level() {
    let repo = Repo::new().await;
    let id = repo.spawn_note(json!({})).await;
    let wait = json!({ "execution_id": id, "seconds": 20 });
    let script = call(&repo.toolset, "wait", wait.clone(), true).await;
    let summary = diff_field(&script);
    let (_, after) = summary_ids(summary.trim_end());
    let names = declared_properties("wait", &repo.toolset);
    assert_declared("wait", &names, &script);

    let status = json!({ "execution_id": id, "diff": "files" });
    let files = call(&repo.toolset, "status", status, true).await;
    let files = diff_field(&files);
    assert!(files.starts_with(summary.trim_end()), "{files}");
    assert!(files.contains("note.txt"), "{files}");

    let direct = call(&repo.toolset, "wait", wait, false).await;
    let blocks = texts(&direct);
    let full = blocks.last().expect("diff block");
    assert!(full.starts_with(&format!("commit {after}")), "{full}");
}

#[tokio::test]
async fn a_command_spawned_at_none_has_nothing_to_show() {
    let repo = Repo::new().await;
    let before = repo.object_count();
    let id = repo.spawn_note(json!({ "diff": "none" })).await;
    let wait = json!({ "execution_id": id, "seconds": 20, "diff": "full" });
    let script = call(&repo.toolset, "wait", wait.clone(), true).await;
    assert_eq!(script["structuredContent"]["status"], json!("exited"));
    assert!(
        script["structuredContent"].get("diff").is_none(),
        "{script}"
    );
    let direct = call(&repo.toolset, "wait", wait, false).await;
    assert_eq!(texts(&direct).len(), 2, "{direct}");
    assert_eq!(repo.object_count(), before);
}

#[tokio::test]
async fn wait_ended_by_a_new_message_reports_stopped_by() {
    let spawner = Spawner::new().await;
    let id = spawner.spawn_id("sleep 30").await;
    let mut context = ToolInvocationContext::default();
    context
        .capabilities
        .insert(CAPABILITY_STRUCTURED_RESULT.to_string());
    let new_message = context.new_message.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        new_message.cancel();
    });
    let result = spawner
        .toolset
        .invoke_with_context(ToolInvocation {
            tool: "wait".to_string(),
            args: json!({ "execution_id": id, "seconds": 20 }),
            context,
            cancel: CancellationToken::new(),
        })
        .await
        .expect("tool result");
    spawner.terminate(&id).await;
    let pending_names = declared_properties("wait", &spawner.toolset);
    assert_declared("wait", &pending_names, &result);
    let fields = &result["structuredContent"];
    assert_eq!(fields["status"], json!("running"), "{result}");
    assert_eq!(fields["stopped_by"], json!("new_message"));
}

fn declared_properties(spec_name: &str, toolset: &BashToolset) -> Vec<String> {
    let schema = toolset
        .tools()
        .into_iter()
        .find(|spec| spec.name == spec_name)
        .and_then(|spec| spec.output_schema().cloned())
        .unwrap_or_else(|| panic!("{spec_name} declares no output schema"));
    schema["properties"]
        .as_object()
        .expect("schema properties")
        .keys()
        .cloned()
        .collect()
}

#[tokio::test]
async fn mapped_tools_declare_their_fields() {
    let directory = tempfile::tempdir().expect("temp dir");
    let toolset = toolset_in(directory.path()).await;
    for spec in toolset.tools() {
        let Some(output) = spec.script_output() else {
            assert!(spec.output_schema().is_none(), "{}", spec.name);
            continue;
        };
        let names = declared_properties(&spec.name, &toolset);
        for named in [&output.stdout, &output.stderr, &output.exit_code]
            .into_iter()
            .flatten()
        {
            assert!(names.contains(named), "{} lacks {named}", spec.name);
        }
    }
}

fn assert_declared(tool: &str, names: &[String], result: &Value) {
    let fields = result["structuredContent"]
        .as_object()
        .unwrap_or_else(|| panic!("{tool} returned no structured content: {result}"));
    for key in fields.keys() {
        assert!(names.contains(key), "{tool} emits undeclared {key}");
    }
}

#[tokio::test]
async fn script_result_fields_match_the_declared_schema() {
    let spawner = Spawner::new().await;
    let exec_names = declared_properties("exec", &spawner.toolset);
    let pending_names = declared_properties("spawn", &spawner.toolset);
    let log_names = declared_properties("read_exec_log", &spawner.toolset);
    let args = json!({ "command": "echo out", "working_dir": spawner.directory.path() });
    assert_declared("exec", &exec_names, &spawner.call("exec", args, true).await);

    let running = spawner.spawn("sleep 30", true).await;
    assert_declared("spawn", &pending_names, &running);
    let id = running["structuredContent"]["execution_id"]
        .as_str()
        .expect("id")
        .to_string();
    let status = spawner
        .call("status", json!({ "execution_id": id }), true)
        .await;
    assert_declared("status", &pending_names, &status);
    spawner.terminate(&id).await;

    let id = log_of(&spawner, "printf 'a\\nb\\n'").await;
    let finished = spawner
        .call("wait", json!({ "execution_id": id, "seconds": 20 }), true)
        .await;
    assert_declared("wait", &pending_names, &finished);
    let args = json!({ "execution_id": id, "stream": "stdout" });
    let log = spawner.call("read_exec_log", args, true).await;
    assert_declared("read_exec_log", &log_names, &log);
}

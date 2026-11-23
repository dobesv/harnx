use super::outcome::{ScriptToolOutcome, ScriptToolTrace, STDOUT_TRUNCATION_MARKER};
use super::provider::{ScriptToolProvider, ScriptToolProviderConfig};
use super::tool_input::ToolInputCommand;
use crate::bash_inner::{InnerTraceEntry, InnerTraceStatus};
use harnx_core::script_tool::ScriptToolOutput;
use harnx_core::tool::{ToolCall, ToolError, ToolProvider, ToolProviderOutput, ToolReplay};
use harnx_script::{ScriptRun, ScriptStatus};
use serde_json::{json, Value};
use std::sync::Arc;

fn run(
    status: ScriptStatus,
    exit_code: i32,
    stdout: &str,
    stderr: &str,
    stdout_truncated: bool,
) -> ScriptRun {
    ScriptRun {
        stdout: stdout.to_string(),
        stderr: stderr.to_string(),
        exit_code,
        stdout_truncated,
        stderr_truncated: false,
        limit_hit: None,
        error: None,
        status,
    }
}

fn outcome(result: ScriptRun, output: ScriptToolOutput) -> ScriptToolOutcome {
    ScriptToolOutcome {
        name: "wrap".to_string(),
        output,
        timeout_secs: 300,
        result,
        aborted: false,
        trace: Default::default(),
        host_stop: None,
    }
}

fn output_value(result: Result<ToolProviderOutput, ToolError>) -> Value {
    match result {
        Ok(output) => output.value,
        Err(ToolError::Recoverable(error)) => {
            panic!("expected Ok output, got recoverable error: {error:#}")
        }
        Err(other) => {
            let kind = match other {
                ToolError::Recoverable(_) => "recoverable",
                ToolError::Fatal(_) => "fatal",
            };
            panic!("unexpected error kind: {kind}")
        }
    }
}

fn recoverable_message(result: Result<ToolProviderOutput, ToolError>) -> (String, Option<Value>) {
    match result {
        Err(ToolError::Recoverable(error)) => {
            let message = format!("{error}");
            let partial = harnx_core::partial_result::partial_result_of(&error).cloned();
            (message, partial)
        }
        other => {
            let kind = match other {
                Ok(_) => "ok",
                Err(ToolError::Fatal(_)) => "fatal",
                Err(ToolError::Recoverable(_)) => "recoverable",
            };
            panic!("expected recoverable error, got {kind}")
        }
    }
}

#[test]
fn text_success_returns_stdout_as_content() {
    let result = outcome(
        run(ScriptStatus::Ok, 0, "hello\nworld\n", "", false),
        ScriptToolOutput::Text,
    )
    .into_provider_result();
    let value = output_value(result);
    assert_eq!(
        value["content"][0]["text"], "hello\nworld\n",
        "stdout must become the result text: {value}"
    );
    assert_eq!(value["status"], "ok");
    assert!(value.get("stderr").is_none(), "empty stderr stays absent");
}

#[test]
fn text_success_keeps_stderr_without_failing() {
    // R7: non-empty stderr with exit 0 is not a failure.
    let result = outcome(
        run(ScriptStatus::Ok, 0, "ok\n", "a warning\n", false),
        ScriptToolOutput::Text,
    )
    .into_provider_result();
    let value = output_value(result);
    assert_eq!(value["status"], "ok");
    assert_eq!(value["stderr"], "a warning\n");
}

#[test]
fn python_host_stop_names_the_recorded_reason() {
    // 4659980781: a terminal fatal inner call stops the Python run; the
    // message the handler recorded in caller-owned state must surface in
    // both the failure message and the payload, not the generic
    // "stopped unexpectedly" text.
    let mut stopped = outcome(
        run(ScriptStatus::Stopped, 130, "", "", false),
        ScriptToolOutput::Text,
    );
    stopped.host_stop =
        Some("bash script exceeded its limit of 10000 tool-call attempts".to_string());
    let (message, partial) = recoverable_message(stopped.into_provider_result());
    assert!(
        message.contains("stopped: bash script exceeded"),
        "the recorded reason must be named: {message}"
    );
    assert_eq!(
        partial.expect("failure payload rides along")["error"],
        json!("bash script exceeded its limit of 10000 tool-call attempts")
    );
}

#[test]
fn json_success_returns_exactly_one_parsed_value() {
    let result = outcome(
        run(
            ScriptStatus::Ok,
            0,
            "{\"title\": \"bug\", \"state\": \"open\"}",
            "",
            false,
        ),
        ScriptToolOutput::Json,
    )
    .into_provider_result();
    let value = output_value(result);
    assert_eq!(value, json!({"title": "bug", "state": "open"}));
}

#[test]
fn json_mode_rejects_malformed_and_multiple_values() {
    for stdout in ["{\"a\": ", "{} {}", ""] {
        let result = outcome(
            run(ScriptStatus::Ok, 0, stdout, "", false),
            ScriptToolOutput::Json,
        )
        .into_provider_result();
        let (message, _) = recoverable_message(result);
        assert!(
            message.contains("printed invalid JSON"),
            "unexpected message: {message}"
        );
    }
}

#[test]
fn non_zero_exit_becomes_error_with_status_and_stderr() {
    let result = outcome(
        run(ScriptStatus::Ok, 2, "", "usage: bad flag\n", false),
        ScriptToolOutput::Text,
    )
    .into_provider_result();
    let (message, partial) = recoverable_message(result);
    assert!(
        message.contains("exited with code 2"),
        "exit status must be in the message: {message}"
    );
    assert!(
        message.contains("usage: bad flag"),
        "bounded stderr must be in the message: {message}"
    );
    let partial = partial.expect("failure payload rides along as partial_result");
    assert_eq!(partial["exit_code"], 2);
    assert_eq!(partial["stderr"], "usage: bad flag\n");
}

#[test]
fn timeout_reports_the_limit() {
    let result = outcome(
        run(ScriptStatus::TimedOut, 124, "partial", "", false),
        ScriptToolOutput::Text,
    )
    .into_provider_result();
    let (message, _) = recoverable_message(result);
    assert!(
        message.contains("timed out after 300s"),
        "unexpected message: {message}"
    );
}

#[test]
fn abort_is_fatal_for_the_batch() {
    let mut outcome = outcome(
        run(ScriptStatus::Cancelled, 130, "", "", false),
        ScriptToolOutput::Text,
    );
    outcome.aborted = true;
    match outcome.into_provider_result() {
        Err(ToolError::Fatal(error)) => {
            assert!(
                format!("{error}").contains("was cancelled"),
                "unexpected fatal message: {error}"
            );
            assert!(
                harnx_core::partial_result::partial_result_of(&error).is_some(),
                "payload must ride along on abort too"
            );
        }
        other => {
            let kind = match other {
                Ok(_) => "ok",
                Err(ToolError::Fatal(_)) => "fatal",
                Err(ToolError::Recoverable(_)) => "recoverable",
            };
            panic!("abort must be fatal, got {kind}")
        }
    }
}

#[test]
fn truncated_text_carries_a_marker() {
    let result = outcome(
        run(ScriptStatus::Ok, 0, "aaaa\nbbbb", "", true),
        ScriptToolOutput::Text,
    )
    .into_provider_result();
    let value = output_value(result);
    let text = value["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains(STDOUT_TRUNCATION_MARKER),
        "truncation marker missing: {text}"
    );
    assert_eq!(value["stdout_truncated"], true);
}

#[test]
fn tool_input_prints_the_arguments_json() {
    let command = ToolInputCommand {
        input: json!({"number": 2813}),
    };
    let output = command.execute(&[]);
    assert_eq!(output.stdout.trim(), "{\"number\":2813}");
    assert_eq!(output.exit_code, 0);

    let usage = command.execute(&["--extra".to_string()]);
    assert_eq!(usage.exit_code, 2);
    assert_eq!(usage.stderr.trim(), "tool_input takes no arguments");
}

/// `expect` needs `Debug`; `ToolError` deliberately has none, so spell out
/// the panic message instead.
fn expect_ok(result: Result<Option<ToolProviderOutput>, ToolError>) -> Option<ToolProviderOutput> {
    match result {
        Ok(value) => value,
        Err(ToolError::Recoverable(error)) | Err(ToolError::Fatal(error)) => {
            panic!("replay dispatch failed: {error:#}")
        }
    }
}

/// C2: a capture-cap cut after a complete JSON value must not let that value
/// silently replace the full output; the failure names the truncation.
#[test]
fn json_mode_rejects_truncated_stdout_even_with_a_valid_prefix() {
    let result = outcome(
        run(ScriptStatus::Ok, 0, "{\"ok\": true}0000", "", true),
        ScriptToolOutput::Json,
    )
    .into_provider_result();
    let (message, partial) = recoverable_message(result);
    assert!(
        message.contains("capture cap"),
        "the failure must name truncation: {message}"
    );
    let partial = partial.expect("failure payload rides along");
    assert_eq!(
        partial["stdout_truncated"], true,
        "the payload must explain why parsing was refused"
    );
}

/// C3: failure payloads carry the inner-call trace in the `bash` tool's
/// shape, plus the dropped-row count and the first failed call.
#[test]
fn failure_payload_carries_the_inner_call_trace() {
    let entry = InnerTraceEntry {
        start_seq: 0,
        call_id: "call-1".to_string(),
        name: "probe_run".to_string(),
        args_summary: "{\"value\":\"x\"}".to_string(),
        status: InnerTraceStatus::ToolError,
        bytes: 12,
        duration_ms: 3,
    };
    let mut failed = outcome(
        run(ScriptStatus::Ok, 1, "", "inner tool failed\n", false),
        ScriptToolOutput::Text,
    );
    failed.trace = ScriptToolTrace {
        entries: vec![entry.clone()],
        dropped: 3,
        first_failed: Some(entry),
    };
    let (_, partial) = recoverable_message(failed.into_provider_result());
    let partial = partial.expect("failure payload rides along");
    assert_eq!(partial["trace"][0]["name"], "probe_run");
    assert_eq!(partial["trace"][0]["status"], "tool_error");
    assert_eq!(partial["trace"][0]["duration_ms"], 3);
    assert_eq!(partial["trace_dropped"], 3);
    assert_eq!(partial["failed_call"]["name"], "probe_run");
    assert_eq!(partial["failed_call"]["call_id"], "call-1");
}

/// C18: replay claims only names this round's accepted view owns, and those
/// answer `interrupted` — a script tool's side effects may have happened, so
/// it is never resumed by a replaying worker.
#[tokio::test]
async fn replay_answers_interrupted_for_owned_names_only() {
    let definition: harnx_core::script_tool::ScriptToolDefinition = serde_json::from_value(json!({
        "name": "wrap",
        "description": "Wrapper.",
        "parameters": {
            "type": "object",
            "properties": {"number": {"type": "integer"}},
            "required": ["number"],
            "additionalProperties": false
        },
        "bash": "true"
    }))
    .unwrap();
    let mut config = crate::config::Config::default();
    config.set_script_tool_definitions(&[definition]).unwrap();
    let config = Arc::new(crate::config::ConfigLock::new(config));
    let provider = ScriptToolProvider::new(ScriptToolProviderConfig {
        tools: config.read().accepted_script_tools(),
        base_providers: Vec::new(),
        config: config.clone(),
        slot: None,
        payload_limit: None,
        attachment_fetcher: None,
        media_store: None,
    });
    let abort = harnx_core::abort::create_abort_signal();

    let owned_call = ToolCall::new("wrap".to_string(), json!({"number": 1}), None, None);
    let replay = ToolReplay {
        session_id: "s",
        tool_round: 1,
        call: &owned_call,
        worker_id: None,
        fence_token: None,
        authorization: None,
    };
    let owned = expect_ok(provider.replay_tool_call(replay, &abort).await)
        .expect("an owned script tool answers interrupted");
    assert_eq!(owned.value["status"], "interrupted");
    assert!(
        owned.value["error"]
            .as_str()
            .unwrap()
            .contains("interrupted"),
        "the diagnosis must be in the payload: {owned:?}"
    );

    let foreign_call = ToolCall::new("fs_read".to_string(), json!({}), None, None);
    let foreign = expect_ok(
        provider
            .replay_tool_call(
                ToolReplay {
                    call: &foreign_call,
                    ..replay
                },
                &abort,
            )
            .await,
    );
    assert!(foreign.is_none(), "unowned names must not be claimed");
}

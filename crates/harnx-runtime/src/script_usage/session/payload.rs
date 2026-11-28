//! Reads one `bash` call's stored output into a [`ScriptRun`].

use harnx_core::partial_result::PARTIAL_RESULT_KEY;
use harnx_core::session::ToolOutput;
use harnx_core::tool::ToolCall;
use serde_json::Value;

use super::{is_error_output, json_len, FailedCall, FailureKind, Round, ScriptRun};
use crate::script_usage::trace::{parse_report, ParsedReport, TraceRow};

const EXIT_NOT_EXECUTABLE: i64 = 126;
const EXIT_NOT_FOUND: i64 = 127;

pub(super) fn script_run(
    round: &Round<'_>,
    call: &ToolCall,
    out: Option<&ToolOutput>,
) -> ScriptRun {
    let mut run = ScriptRun {
        seq: round.seq,
        timestamp: round.timestamp,
        status: "no_result".to_string(),
        exit_code: None,
        failure: Some(FailureKind::NoResult),
        failed_call: None,
        inner: Vec::new(),
        trace_dropped: 0,
        unparsed_trace_lines: 0,
        script_bytes: call
            .arguments
            .get("script")
            .and_then(Value::as_str)
            .map_or(0, |s| s.len() as u64),
        result_bytes: 0,
        output_truncated: false,
    };
    let Some(out) = out else { return run };
    run.result_bytes = json_len(&out.output);
    let errored = is_error_output(&out.output);
    let payload = if errored {
        out.output.get(PARTIAL_RESULT_KEY)
    } else {
        Some(&out.output)
    };
    match payload.filter(|p| p.get("status").and_then(Value::as_str).is_some()) {
        Some(payload) => fill_from_payload(&mut run, payload),
        None => {
            run.status = if errored { "error" } else { "unknown" }.to_string();
            run.failure = errored.then_some(FailureKind::NonZeroExit);
        }
    }
    run
}

fn fill_from_payload(run: &mut ScriptRun, payload: &Value) {
    let report = parse_report(report_text(payload));
    run.status = text_field(payload, "status");
    run.exit_code = payload.get("exit_code").and_then(Value::as_i64);
    run.trace_dropped = payload
        .get("trace_dropped")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    run.output_truncated = ["stdout_truncated", "stderr_truncated"]
        .iter()
        .any(|key| payload.get(*key).and_then(Value::as_bool) == Some(true));
    run.failed_call = payload
        .get("failed_call")
        .filter(|v| v.is_object())
        .map(|v| FailedCall {
            name: text_field(v, "name"),
            status: text_field(v, "status"),
        });
    run.failure = classify(payload, run, &report);
    match payload.get("trace").and_then(Value::as_array) {
        Some(trace) => run.inner = trace.iter().map(trace_row).collect(),
        None => {
            run.unparsed_trace_lines = report.unparsed_lines;
            run.inner = report.rows;
        }
    }
}

fn report_text(payload: &Value) -> &str {
    payload
        .get("content")
        .and_then(|c| c.get(0))
        .and_then(|part| part.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn text_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn trace_row(element: &Value) -> TraceRow {
    let number = |key: &str| element.get(key).and_then(Value::as_u64).unwrap_or(0);
    TraceRow {
        name: text_field(element, "name"),
        status: text_field(element, "status"),
        count: 1,
        bytes: number("bytes"),
        duration_ms: number("duration_ms"),
    }
}

/// A script that succeeded has no failure whatever else its payload holds:
/// it may have caught a Python error or guarded a failing inner call. For one
/// that did not, the first rule that applies wins; the order is part of the
/// report's meaning.
fn classify(payload: &Value, run: &ScriptRun, report: &ParsedReport) -> Option<FailureKind> {
    if run.status == "ok" && run.exit_code == Some(0) {
        return None;
    }
    let by_status = match run.status.as_str() {
        "timed_out" => Some(FailureKind::TimedOut),
        "cancelled" => Some(FailureKind::Cancelled),
        "stopped" => Some(FailureKind::Stopped),
        "interrupted" => Some(FailureKind::Interrupted),
        _ => None,
    };
    let candidates = [
        by_status,
        payload
            .get("limit_hit")
            .and_then(Value::as_str)
            .map(|_| FailureKind::LimitHit),
        (run.exit_code == Some(EXIT_NOT_EXECUTABLE)).then_some(FailureKind::Exit126),
        (run.exit_code == Some(EXIT_NOT_FOUND)).then_some(FailureKind::Exit127),
        run.failed_call
            .as_ref()
            .map(|_| FailureKind::InnerCallFailed),
        report
            .python_type_error
            .then_some(FailureKind::PythonTypeError),
        report.python_traceback.then_some(FailureKind::PythonError),
    ];
    candidates
        .into_iter()
        .flatten()
        .next()
        .or(Some(FailureKind::NonZeroExit))
}

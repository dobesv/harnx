use anyhow::anyhow;
use harnx_core::partial_result::error_with_partial_result;
use harnx_core::safety::{sanitize_output_text, truncate_output, TruncateOpts};
use harnx_core::script_tool::ScriptToolOutput;
use harnx_core::tool::{ToolError, ToolProviderOutput};
use serde_json::{json, Value};

use crate::bash_inner::{InnerTraceEntry, InnerTraceStatus};

/// Marker appended to `output: text` results whose stdout was cut by the
/// backend capture cap, so the model can tell partial output from complete.
pub(super) const STDOUT_TRUNCATION_MARKER: &str =
    "[stdout truncated at the script output cap; discarded output is unavailable]";

/// The run's inner-call trace, captured from the run's `InnerCallRun` after
/// the body finished. Carried into failure payloads with the same fields the
/// `bash` tool reports, so a failed script tool is as diagnosable as a failed
/// `bash` script.
#[derive(Default)]
pub(super) struct ScriptToolTrace {
    /// Finalized trace entries in start order.
    pub(super) entries: Vec<InnerTraceEntry>,
    /// Entries dropped by the trace retention cap; only the count survives.
    pub(super) dropped: usize,
    /// First failed call, kept even when its trace row was dropped.
    pub(super) first_failed: Option<InnerTraceEntry>,
}

/// One finished script-tool run, classified per R7 and decision 7.
pub(super) struct ScriptToolOutcome {
    pub(super) name: String,
    pub(super) output: ScriptToolOutput,
    pub(super) timeout_secs: u64,
    pub(super) result: harnx_script::ScriptRun,
    pub(super) aborted: bool,
    pub(super) trace: ScriptToolTrace,
    /// Why a Python body stopped the host: a terminal `InnerOutcome::Fatal`
    /// records its message before calling `ScriptControl::stop()`, and this
    /// carries it into the failure message and payload. Bash bodies report
    /// their fatal through `ScriptRun.error`, so this stays `None` there.
    pub(super) host_stop: Option<String>,
}

impl ScriptToolOutcome {
    fn status(&self) -> &'static str {
        if self.aborted {
            return "cancelled";
        }
        match self.result.status {
            harnx_script::ScriptStatus::Ok => "ok",
            harnx_script::ScriptStatus::Error => "error",
            harnx_script::ScriptStatus::TimedOut => "timed_out",
            harnx_script::ScriptStatus::Cancelled => "cancelled",
            harnx_script::ScriptStatus::Stopped => "stopped",
        }
    }

    fn succeeded(&self) -> bool {
        !self.aborted
            && self.result.status == harnx_script::ScriptStatus::Ok
            && self.result.exit_code == 0
    }

    /// Sanitize and bound one captured stream for the result payload.
    fn bounded_stream(text: &str) -> String {
        truncate_output(&sanitize_output_text(text), &TruncateOpts::default())
    }

    /// The status half of the failure message (R7).
    fn status_message(&self) -> String {
        if self.aborted || self.result.status == harnx_script::ScriptStatus::Cancelled {
            return format!("script tool '{}' was cancelled", self.name);
        }
        match self.result.status {
            harnx_script::ScriptStatus::TimedOut => format!(
                "script tool '{}' timed out after {timeout_secs}s",
                self.name,
                timeout_secs = self.timeout_secs
            ),
            harnx_script::ScriptStatus::Stopped => match &self.host_stop {
                Some(reason) => format!("script tool '{}' stopped: {reason}", self.name),
                None => format!(
                    "script tool '{}' stopped unexpectedly; this tool does not support host stops",
                    self.name
                ),
            },
            _ => format!(
                "script tool '{}' exited with code {}",
                self.name, self.result.exit_code
            ),
        }
    }

    /// The failure message: what went wrong plus bounded stderr (R7).
    fn message(&self) -> String {
        format!(
            "{}{}",
            self.status_message(),
            Self::stderr_suffix(&self.result.stderr)
        )
    }

    /// `"; stderr: …"` when the bounded stderr is non-empty, else nothing.
    // qual:allow(iosp) — one emptiness branch picks between two format calls; splitting the branch from the calls just renames the same two lines
    fn stderr_suffix(stderr: &str) -> String {
        let bounded = Self::bounded_stream(stderr);
        if bounded.trim().is_empty() {
            String::new()
        } else {
            format!("; stderr: {bounded}")
        }
    }

    /// Diagnostics for the error path. The engine attaches this as
    /// `partial_result` on the error result, so the exit status, streams,
    /// stream truncation flags and the inner-call trace stay visible to the
    /// model.
    fn failure_payload(&self) -> Value {
        let mut payload = json!({
            "status": self.status(),
            "exit_code": self.result.exit_code,
            "stdout": Self::bounded_stream(&self.result.stdout),
            "stderr": Self::bounded_stream(&self.result.stderr),
            "stdout_truncated": self.result.stdout_truncated,
            "stderr_truncated": self.result.stderr_truncated,
            "limit_hit": self.result.limit_hit,
            "error": self
                .result
                .error
                .as_deref()
                .map(sanitize_output_text)
                .or_else(|| self.host_stop.as_deref().map(sanitize_output_text)),
            "trace": Self::trace_json(&self.trace.entries),
        });
        if self.trace.dropped > 0 {
            // Inner calls over the trace retention cap; their rows are gone,
            // so the count is the only record left.
            payload["trace_dropped"] = json!(self.trace.dropped);
        }
        if let Some(failed_call) = self.failed_call() {
            payload["failed_call"] = failed_call;
        }
        payload
    }

    /// Trace rows in the same shape as the `bash` tool's payload.
    fn trace_json(entries: &[InnerTraceEntry]) -> Vec<Value> {
        entries
            .iter()
            .map(|entry| {
                json!({
                    "call_id": entry.call_id,
                    "name": entry.name,
                    "args_summary": entry.args_summary,
                    "status": entry.status.as_str(),
                    "bytes": entry.bytes,
                    "duration_ms": entry.duration_ms,
                })
            })
            .collect()
    }

    /// The first failed inner call: the run's retained `first_failed`, or the
    /// first non-Ok/non-Cancelled trace row when the run recorded none.
    fn failed_call(&self) -> Option<Value> {
        let entry = self.trace.first_failed.as_ref().or_else(|| {
            self.trace.entries.iter().find(|entry| {
                !matches!(
                    entry.status,
                    InnerTraceStatus::Ok | InnerTraceStatus::Cancelled
                )
            })
        })?;
        Some(json!({
            "name": entry.name,
            "args_summary": entry.args_summary,
            "status": entry.status.as_str(),
            "call_id": entry.call_id,
        }))
    }

    /// Abort is terminal for the whole batch, matching `BashProvider`;
    /// anything else is a normal recoverable tool error.
    fn error_result(&self, fatal: bool) -> ToolError {
        let error =
            error_with_partial_result(anyhow!("{}", self.message()), self.failure_payload());
        if fatal {
            ToolError::Fatal(error)
        } else {
            ToolError::Recoverable(error)
        }
    }

    /// R7 text result: stdout as the content text, with bounded stderr kept
    /// alongside (never failing the run) and a marker when the cap cut stdout.
    fn text_result(&self) -> ToolProviderOutput {
        ToolProviderOutput::new(self.text_payload())
    }

    // qual:allow(iosp) — each branch is one optional payload field of the R7 output contract; the branches and the json! calls are the same concern
    fn text_payload(&self) -> Value {
        let mut payload = json!({
            "status": "ok",
            "exit_code": self.result.exit_code,
            "limit_hit": self.result.limit_hit,
            "content": [{"type": "text", "text": self.displayed_stdout()}],
        });
        let stderr = Self::bounded_stream(&self.result.stderr);
        if !stderr.trim().is_empty() {
            // Non-empty stderr with exit 0 is not a failure (R7), but it
            // would otherwise be invisible to the model.
            payload["stderr"] = json!(stderr);
        }
        if self.result.stdout_truncated {
            payload["stdout_truncated"] = json!(true);
        }
        payload
    }

    /// Sanitized stdout, carrying the truncation marker when the cap cut it
    /// (decision 7: keep the truncated text and say so).
    fn displayed_stdout(&self) -> String {
        let mut stdout = sanitize_output_text(&self.result.stdout);
        if !self.result.stdout_truncated {
            return stdout;
        }
        if !stdout.is_empty() && !stdout.ends_with('\n') {
            stdout.push('\n');
        }
        stdout.push_str(STDOUT_TRUNCATION_MARKER);
        stdout
    }

    /// R7 JSON result: exactly one complete JSON value on stdout (decision 7);
    /// trailing content, empty output, multiple values, and output cut by
    /// the capture cap are errors. The truncation flag is checked before
    /// parsing: a cut stream can end right after a valid JSON value, and that
    /// value must not silently replace the complete output.
    fn json_result(&self) -> Result<ToolProviderOutput, ToolError> {
        if self.result.stdout_truncated {
            return Err(ToolError::Recoverable(error_with_partial_result(
                anyhow!(
                    "script tool '{}' printed past the stdout capture cap; json output requires the complete value",
                    self.name
                ),
                self.failure_payload(),
            )));
        }
        let stdout = self.result.stdout.trim();
        match serde_json::from_str::<Value>(stdout) {
            Ok(value) => Ok(ToolProviderOutput::new(value)),
            Err(error) => Err(ToolError::Recoverable(error_with_partial_result(
                anyhow!("script tool '{}' printed invalid JSON: {error}", self.name),
                self.failure_payload(),
            ))),
        }
    }

    pub(super) fn into_provider_result(self) -> Result<ToolProviderOutput, ToolError> {
        if self.aborted {
            return Err(self.error_result(true));
        }
        if !self.succeeded() {
            return Err(self.error_result(false));
        }
        match self.output {
            ScriptToolOutput::Text => Ok(self.text_result()),
            ScriptToolOutput::Json => self.json_result(),
        }
    }
}

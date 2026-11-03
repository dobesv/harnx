//! Script mode's `bash` provider (#2537).
//!
//! `bash` is script mode's tool name. While an agent's `bash_tools` resolves
//! to a non-empty inner set, the name is reserved: the model sees the
//! synthesized declaration (config::bash_declaration) and a `bash` call must
//! never fall through to an external tool of the same name — NATS registers
//! raw-name aliases and would win provider dispatch otherwise. This provider
//! claims the name at index 0 of `build_tool_providers`, ahead of the NATS
//! provider, and runs the BashKit backend with the inner-call executor.

use std::sync::{Arc, Mutex, Weak};

use anyhow::anyhow;
use async_trait::async_trait;
use harnx_core::abort::AbortSignal;
use harnx_core::partial_result::error_with_partial_result;
use harnx_core::safety::{sanitize_output_text, truncate_output, TruncateOpts};
use harnx_core::tool::{ToolError, ToolProgress, ToolProvider, ToolProviderOutput, ToolReplay};
use harnx_engine::tool::ToolEvalContext;
use harnx_script::{
    commands::{tool_commands, ToolSpec},
    BashKitBackend, CancellationToken, ScriptBackend, ScriptLimits,
};
use serde::Serialize;
use serde_json::{json, Value};

use crate::bash_inner::{
    InnerCallExecutor, InnerCallExecutorConfig, InnerProgressAggregator, InnerTraceStatus,
    PayloadLimit, ScriptInnerExecutor,
};
use crate::config::BASH_TOOL_NAME;

/// Default `timeout_secs` for the `bash` tool, matching `bash_exec`.
pub const DEFAULT_BASH_TIMEOUT_SECS: u64 = 86_400;

/// Per-round handoff from the runtime to the provider (C6). The provider is
/// constructed before the round's `ToolEvalContext` exists, and the provider
/// trait has no context parameter, so the runtime builds the context, wraps
/// it in an `Arc`, and stores a weak reference here. The weak reference is
/// what breaks the context → provider → slot → context cycle: when the round
/// ends, the context and its providers drop together and the slot dangles
/// harmlessly. A missing slot means this execution path never offered a
/// context (operator contexts, orphan-repair reruns), and `bash` fails
/// closed with a recoverable error instead of dispatching.
#[derive(Default)]
pub struct BashEvalContextSlot(Mutex<Option<Weak<ToolEvalContext>>>);

impl BashEvalContextSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(None)))
    }

    pub fn set(&self, context: &Arc<ToolEvalContext>) {
        *self.0.lock().expect("bash context slot lock") = Some(Arc::downgrade(context));
    }

    fn get(&self) -> Option<Arc<ToolEvalContext>> {
        self.0
            .lock()
            .expect("bash context slot lock")
            .as_ref()
            .and_then(Weak::upgrade)
    }
}

/// The in-process `bash` tool. One instance per tool round, built in
/// `build_tool_providers` with the round's resolved inner set and the base
/// provider list (everything except this provider, so recursive `bash`
/// dispatch is structurally impossible).
pub struct BashProvider {
    /// Captured once at provider construction (R2-3): the reservation flag
    /// must not flip between declaration exposure and execution.
    active: bool,
    specs: Vec<ToolSpec>,
    executor: InnerCallExecutor,
    slot: Option<Arc<BashEvalContextSlot>>,
    backend: BashKitBackend,
}

/// Inputs resolved before a round's evaluation context exists.
pub struct BashProviderConfig {
    pub active: bool,
    pub inner_declarations: Vec<harnx_core::tool::ToolDeclaration>,
    pub base_providers: Vec<Arc<dyn ToolProvider>>,
    pub slot: Option<Arc<BashEvalContextSlot>>,
    pub payload_limit: Option<PayloadLimit>,
}

struct ScriptInvocation<'a> {
    tool_round: Option<u64>,
    abort: &'a AbortSignal,
    progress: Arc<dyn ToolProgress>,
}

impl BashProvider {
    pub fn new(config: BashProviderConfig) -> Self {
        let BashProviderConfig {
            active,
            inner_declarations,
            base_providers,
            slot,
            payload_limit,
        } = config;
        let allowed = inner_declarations
            .iter()
            .map(|declaration| declaration.name.clone())
            .collect();
        let mut config = InnerCallExecutorConfig::new(allowed, base_providers);
        if let Some(payload_limit) = payload_limit {
            config = config.with_payload_limit(payload_limit);
        }
        let specs = inner_declarations
            .iter()
            .map(|declaration| ToolSpec {
                name: declaration.name.clone(),
                description: declaration.description.clone(),
                schema: declaration.parameters.as_value().clone(),
            })
            .collect();
        Self {
            active,
            specs,
            executor: InnerCallExecutor::new(config),
            slot,
            backend: BashKitBackend,
        }
    }

    fn context_slot(&self) -> Result<Arc<ToolEvalContext>, ToolError> {
        self.slot
            .as_ref()
            .and_then(|slot| slot.get())
            .ok_or_else(|| {
                ToolError::Recoverable(anyhow!(
                    "the reserved '{BASH_TOOL_NAME}' tool cannot run scripts in this execution context"
                ))
            })
    }

    async fn run_script(
        &self,
        arguments: Value,
        invocation: ScriptInvocation<'_>,
    ) -> Result<ToolProviderOutput, ToolError> {
        let ScriptInvocation {
            tool_round,
            abort,
            progress,
        } = invocation;
        let params: BashToolParams = serde_json::from_value(arguments)
            .map_err(|error| ToolError::Recoverable(anyhow!("invalid bash arguments: {error}")))?;
        let script = params
            .script
            .clone()
            .ok_or_else(|| ToolError::Recoverable(anyhow!("bash requires a 'script' string")))?;

        let context = self.context_slot()?;
        let aggregator = Arc::new(InnerProgressAggregator::new(progress));
        let run = Arc::new(
            self.executor
                .begin_run(&context, tool_round, Some(aggregator)),
        );
        let adapter = Arc::new(ScriptInnerExecutor::new(Arc::clone(&run), abort.clone()));
        let commands = tool_commands(self.specs.clone(), adapter).map_err(|error| {
            ToolError::Recoverable(anyhow!("bash tool commands failed to register: {error}"))
        })?;

        let limits = ScriptLimits {
            timeout_secs: params.timeout_secs.unwrap_or(DEFAULT_BASH_TIMEOUT_SECS),
            ..ScriptLimits::default()
        };
        // Bridge the parent abort into the backend's cancellation token: no
        // detached watcher, the provider future owns the select. On abort we
        // cancel the token and still await the backend so pending local work
        // is dropped and the trace keeps its cancelled rows.
        let token = CancellationToken::new();
        let run_fut = self.backend.run(harnx_script::ScriptRequest {
            script: &script,
            limits,
            commands,
            fs_mounts: Vec::new(),
            cancel: token.clone(),
        });
        tokio::pin!(run_fut);
        let result = tokio::select! {
            result = &mut run_fut => result,
            _ = harnx_core::abort::wait_abort_signal(abort) => {
                token.cancel();
                (&mut run_fut).await
            }
        };

        let outcome = BashOutcome::new(result, abort, &run, params);
        outcome.into_provider_result()
    }
}

struct BashOutcome {
    status: &'static str,
    exit_code: i32,
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
    limit_hit: Option<String>,
    trace: Vec<crate::bash_inner::InnerTraceEntry>,
    /// Inner calls admitted over the trace-retention cap (256); their rows
    /// were dropped from `trace`, so the count is the only record left.
    trace_dropped: usize,
    /// First failed call of the run, kept separately from the trace so it
    /// survives trace retention (PR3 review F13).
    first_failed: Option<crate::bash_inner::InnerTraceEntry>,
    params: BashToolParams,
    aborted: bool,
}

struct RenderedStream<'a> {
    text: &'a str,
    truncated: bool,
}

impl BashOutcome {
    fn new(
        result: harnx_script::ScriptRun,
        abort: &AbortSignal,
        run: &crate::bash_inner::InnerCallRun,
        params: BashToolParams,
    ) -> Self {
        let trace = run.trace();
        let trace_dropped = run.dropped_trace_entries();
        let first_failed = run.first_failed();
        let aborted = abort.aborted();
        let status = if aborted {
            "cancelled"
        } else {
            match result.status {
                harnx_script::ScriptStatus::Ok => "ok",
                harnx_script::ScriptStatus::Error => "error",
                harnx_script::ScriptStatus::TimedOut => "timed_out",
                harnx_script::ScriptStatus::Cancelled => "cancelled",
            }
        };
        Self {
            status,
            exit_code: result.exit_code,
            stdout: result.stdout,
            stderr: result.stderr,
            stdout_truncated: result.stdout_truncated,
            stderr_truncated: result.stderr_truncated,
            limit_hit: result.limit_hit,
            trace,
            trace_dropped,
            first_failed,
            params,
            aborted,
        }
    }

    fn truncate_opts(&self) -> TruncateOpts {
        // Same defaults as `bash_exec` (`truncate_opts_from`): head/tail
        // window plus a byte cap, per output stream.
        let defaults = TruncateOpts::default();
        TruncateOpts {
            head_lines: self.params.head_lines.unwrap_or(defaults.head_lines),
            tail_lines: self.params.tail_lines.unwrap_or(defaults.tail_lines),
            max_output_bytes: self
                .params
                .max_output_bytes
                .unwrap_or(defaults.max_output_bytes),
            ..defaults
        }
    }

    /// First inner call that did not complete successfully, for the failure
    /// payload. Trace statuses are engine/transport statuses; the command
    /// layer's own error text is on stderr. The run's separately kept first
    /// failure wins: trace retention drops the oldest rows, so a failure
    /// early in a >256-call run can be absent from the trace itself
    /// (PR3 review F13).
    fn failed_call(&self) -> Option<Value> {
        let entry = self.first_failed.as_ref().or_else(|| {
            self.trace.iter().find(|entry| {
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

    fn trace_json(&self) -> Vec<Value> {
        self.trace
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

    fn message(&self) -> String {
        match self.status {
            "ok" => format!("bash script completed with exit code {}", self.exit_code),
            "timed_out" => format!(
                "bash script timed out after {}s",
                self.params
                    .timeout_secs
                    .unwrap_or(DEFAULT_BASH_TIMEOUT_SECS)
            ),
            "cancelled" => "bash script was cancelled".to_string(),
            _ => format!("bash script failed with exit code {}", self.exit_code),
        }
    }

    fn payload(&self) -> Value {
        let opts = self.truncate_opts();
        // Ported from `harnx-bash-tools/src/server/render.rs`
        // (`render_streams_block`): sanitize + truncate once, reuse for both
        // the JSON fields and the rendered report.
        let stdout_sanitized = sanitize_output_text(&self.stdout);
        let stdout = truncate_output(&stdout_sanitized, &opts);
        let stderr_sanitized = sanitize_output_text(&self.stderr);
        let stderr = truncate_output(&stderr_sanitized, &opts);
        let stdout_truncated = self.stdout_truncated || stdout != stdout_sanitized;
        let stderr_truncated = self.stderr_truncated || stderr != stderr_sanitized;
        let rendered = self.render(
            RenderedStream {
                text: &stdout,
                truncated: stdout_truncated,
            },
            RenderedStream {
                text: &stderr,
                truncated: stderr_truncated,
            },
        );
        let mut payload = json!({
            "status": self.status,
            "exit_code": self.exit_code,
            "stdout": stdout,
            "stderr": stderr,
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
            "limit_hit": self.limit_hit,
            "trace": self.trace_json(),
            // `content` text mirrors the native toolset envelope so the
            // display markdown falls back to the rendered report.
            "content": [{"type": "text", "text": rendered}],
        });
        if self.trace_dropped > 0 {
            // Inner calls over the 256-row trace cap; their rows are gone,
            // so the count is the only record left (PR3 review F13).
            payload["trace_dropped"] = json!(self.trace_dropped);
        }
        if let Some(failed_call) = self.failed_call() {
            payload["failed_call"] = failed_call;
        }
        payload
    }

    /// YAML front matter + fenced stdout/stderr + trace lines, in the
    /// `bash_exec` report shape.
    fn render(&self, stdout: RenderedStream<'_>, stderr: RenderedStream<'_>) -> String {
        #[derive(Serialize)]
        struct YamlHeader<'a> {
            status: &'a str,
            exit_code: i32,
            #[serde(skip_serializing_if = "Option::is_none")]
            limit_hit: Option<&'a str>,
            total_lines: usize,
            total_bytes: usize,
        }
        // `bash_exec` sums stdout+stderr for the header totals
        // (`harnx-bash-tools/src/server/exec.rs`); a stderr-only failure
        // must not report 0/0 (PR3 review F10).
        let total_lines = self.stdout.lines().count() + self.stderr.lines().count();
        let total_bytes = self.stdout.len() + self.stderr.len();
        let yaml = serde_yaml::to_string(&YamlHeader {
            status: self.status,
            exit_code: self.exit_code,
            limit_hit: self.limit_hit.as_deref(),
            total_lines,
            total_bytes,
        })
        .unwrap_or_default();
        let mut out = format!("```yaml\n{yaml}```\n");
        out.push_str(&render_stream_block(
            "stdout",
            stdout.text,
            stdout.truncated,
        ));
        out.push('\n');
        out.push_str(&render_stream_block(
            "stderr",
            stderr.text,
            stderr.truncated,
        ));
        if !self.trace.is_empty() {
            out.push_str("\n## inner call trace\n");
            for entry in &self.trace {
                out.push_str(&format!("- {}\n", entry.to_progress_line()));
            }
            if self.trace_dropped > 0 {
                out.push_str(&format!(
                    "- … {trace_dropped} further inner call(s) beyond the retained-trace cap\n",
                    trace_dropped = self.trace_dropped
                ));
            }
        }
        out
    }

    fn into_provider_result(self) -> Result<ToolProviderOutput, ToolError> {
        let message = self.message();
        let payload = self.payload();
        if self.aborted {
            // Parent abort is terminal for the whole batch, per the provider
            // contract; the payload rides along in the error chain for
            // handlers that look for it.
            return Err(ToolError::Fatal(error_with_partial_result(
                anyhow!("{message}"),
                payload,
            )));
        }
        match self.status {
            "ok" if self.exit_code == 0 => Ok(ToolProviderOutput::new(payload)),
            _ => Err(ToolError::Recoverable(error_with_partial_result(
                anyhow!("{message}"),
                payload,
            ))),
        }
    }
}

/// One stream's fenced block with HTML markers, mirroring
/// `harnx-bash-tools/src/server/render.rs::render_stream_block`. Content is
/// already sanitized and truncated by the caller; this only formats.
fn render_stream_block(name: &str, content: &str, truncated: bool) -> String {
    let mut block = format!("<!-- start {name} -->\n```\n{content}");
    if truncated {
        block.push_str(&format!(
            "\n\n[{name} truncated. Use max_output_bytes, head_lines, or tail_lines to see more]"
        ));
    }
    block.push_str(&format!("\n```\n<!-- end {name} -->"));
    block
}

#[derive(Debug, Default, serde::Deserialize)]
struct BashToolParams {
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    head_lines: Option<usize>,
    #[serde(default)]
    tail_lines: Option<usize>,
    #[serde(default)]
    max_output_bytes: Option<usize>,
}

#[async_trait]
impl ToolProvider for BashProvider {
    fn name(&self) -> &str {
        "bash-script"
    }

    fn has_tool(&self, tool_name: &str) -> bool {
        self.active && tool_name == BASH_TOOL_NAME
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
        abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        if !self.has_tool(tool_name) {
            return Err(ToolError::Recoverable(anyhow!(
                "provider does not handle tool '{tool_name}'"
            )));
        }
        let origin = harnx_core::tool::ToolCallOrigin::default();
        self.run_script(
            arguments,
            ScriptInvocation {
                tool_round: origin.tool_round,
                abort,
                progress: Arc::new(NoopProgress),
            },
        )
        .await
    }

    async fn call_tool_with_progress(
        &self,
        tool_name: &str,
        arguments: Value,
        origin: harnx_core::tool::ToolCallOrigin<'_>,
        abort: &AbortSignal,
        progress: Arc<dyn ToolProgress>,
    ) -> Result<ToolProviderOutput, ToolError> {
        if !self.has_tool(tool_name) {
            return Err(ToolError::Recoverable(anyhow!(
                "provider does not handle tool '{tool_name}'"
            )));
        }
        self.run_script(
            arguments,
            ScriptInvocation {
                tool_round: origin.tool_round,
                abort,
                progress,
            },
        )
        .await
    }

    /// A `bash` invocation is never re-run on worker loss: side effects may
    /// have already happened, and only the root call's payload is durable
    /// (there are no inner-call rows to rebuild from). When script mode owns
    /// `bash`, returning `Some` answers `interrupted` — a final observation
    /// the model can act on — and wins over NATS replay by provider order
    /// (this provider is first in `build_tool_providers`). Only when the
    /// provider is active: an inactive agent has no resolved `bash_tools`,
    /// so its bare `bash` name belongs to a tool-server-native tool and must
    /// keep normal NATS journal recovery (PR3 review F9).
    async fn replay_tool_call(
        &self,
        replay: ToolReplay<'_>,
        _abort: &AbortSignal,
    ) -> Result<Option<ToolProviderOutput>, ToolError> {
        if !self.active || replay.call.name != BASH_TOOL_NAME {
            return Ok(None);
        }
        let message = "bash script was interrupted: the worker restarted before results were \
persisted; the script was not resumed and its side effects may have happened";
        Ok(Some(ToolProviderOutput::new(json!({
            "status": "interrupted",
            "error": message,
            "content": [{"type": "text", "text": message}],
        }))))
    }
}

struct NoopProgress;

impl ToolProgress for NoopProgress {
    fn update(&self, _patch: harnx_core::tool::ToolUpdatePatch) {}
}

#[cfg(test)]
mod tests;

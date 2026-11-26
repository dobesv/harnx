//! In-process lifecycle event hooks.
//!
//! Runs the resolved per-event hook chains from
//! `Config::resolved_event_hooks` through the same BashKit/Monty script
//! runtime the tool middleware uses. Scripts see `event_input`, may `fetch`
//! allowlisted URLs, and record effects with the event's operations:
//! `add_context` (SessionStart, UserPromptSubmit), `block`/`rewrite_prompt`
//! (UserPromptSubmit), and `resume` (Stop). Entries that declare `tools:`
//! also get those tools as script commands and as `tools.NAME(...)` in
//! Python, through the shared bridge in `script_tools` (see "Calling tools" in
//! `docs/hooks-guide.md`). Interaction operations
//! (`confirm`, `save`, `next`, …) are intentionally not registered: an event
//! hook that tries one fails, which the hook's `on_error` policy handles.
//!
//! Composition is sequential and ordered: contexts accumulate in declaration
//! order, a later `rewrite_prompt` replaces an earlier one and the rewritten
//! prompt is what following hooks see, `block` stops the chain and discards
//! all accumulated context, and the first `resume` wins — remaining Stop hooks
//! are skipped. A failed run's effects are dropped entirely; `on_error:
//! allow` warns and continues, `deny` (SessionStart/UserPromptSubmit only)
//! fails the event.

mod operations;
mod tools;

use tools::ToolContext;

use crate::config::{GlobalConfig, ResolvedEventHook};
use anyhow::{anyhow, Result};
use harnx_core::abort::wait_abort_signal;
use harnx_core::hooks::{EventHookOnError, HookEventKind};
use harnx_engine::tool::ToolEvalContext;
use harnx_script::{
    BashKitBackend, CancellationToken, ScriptBackend, ScriptLimits, ScriptRequest, ScriptStatus,
};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

use crate::utils::AbortSignal;

/// The payload each hook's script reads through `event_input`.
#[derive(Debug, Clone)]
pub struct EventHookInput {
    pub event: HookEventKind,
    pub session_id: String,
    pub cwd: PathBuf,
    pub resume_count: u32,
    /// UserPromptSubmit: the prompt as the hook sees it — the rewritten text
    /// once an earlier hook in the chain rewrote it.
    pub prompt: Option<String>,
    /// SessionStart provenance, mirroring the legacy payload.
    pub source: Option<String>,
    pub model: Option<String>,
    /// Stop: the assistant message that ended the turn.
    pub last_assistant_message: Option<String>,
    /// StopFailure diagnostics.
    pub error: Option<String>,
    pub error_type: Option<String>,
}

impl Default for EventHookInput {
    fn default() -> Self {
        Self {
            event: HookEventKind::SessionStart,
            session_id: String::new(),
            cwd: PathBuf::new(),
            resume_count: 0,
            prompt: None,
            source: None,
            model: None,
            last_assistant_message: None,
            error: None,
            error_type: None,
        }
    }
}

impl EventHookInput {
    /// The JSON payload `event_input` serves to hook scripts.
    pub fn to_json(&self) -> Value {
        let mut payload = serde_json::Map::new();
        payload.insert("event".into(), json!(self.event.key()));
        payload.insert("session_id".into(), json!(self.session_id));
        payload.insert("cwd".into(), json!(self.cwd.display().to_string()));
        payload.insert("resume_count".into(), json!(self.resume_count));
        for (key, value) in [
            ("prompt", &self.prompt),
            ("source", &self.source),
            ("model", &self.model),
            ("last_assistant_message", &self.last_assistant_message),
            ("error", &self.error),
            ("error_type", &self.error_type),
        ] {
            if let Some(value) = value {
                payload.insert(key.into(), json!(value));
            }
        }
        Value::Object(payload)
    }
}

/// Aggregated effects of one event's hook chain.
#[derive(Debug, Default, Clone)]
pub struct EventHookOutcome {
    /// `add_context` texts in chain order.
    pub context: Vec<String>,
    /// UserPromptSubmit `block` reason. Set only when a hook blocked.
    pub block: Option<String>,
    /// The last `rewrite_prompt` text, if any hook rewrote.
    pub rewrite: Option<String>,
    /// The first `resume` message (may be empty: resume without a message).
    pub resume: Option<String>,
}

impl EventHookOutcome {
    pub fn context_text(&self) -> Option<String> {
        (!self.context.is_empty()).then(|| self.context.join("\n\n"))
    }
}

/// Effects recorded by one hook run. Dropped when the run fails.
#[derive(Debug, Default, Clone)]
struct RunState {
    context: Vec<String>,
    block: Option<String>,
    rewrite: Option<String>,
    resume: Option<String>,
}

/// Run one event's hooks in-process.
///
/// `tool_context` is called at most once, only if an entry for this event declares tools.
/// It must refresh discovered declarations in `config` before returning the base
/// context. Use a factory returning a boxed future to keep the large context
/// builder off the agent loop's stack, without allocating for tool-free hooks.
/// An eagerly constructed (unboxed) future here overflowed Tokio's default
/// stack in existing worker middleware tests even with no hooks configured —
/// new dispatch sites must keep the `|| Box::pin(...)` pattern.
/// Before scripts run, shared context setup gets a separate budget: the maximum
/// `timeout` among this event's entries with nonempty `tools`. Expiry drops the
/// setup future without aborting the turn or retrying. Tool-declaring entries
/// then fail at their normal chain positions according to `on_error`; tool-free
/// entries still run unless block, resume or deny stops the chain. Each script
/// retains its full per-entry timeout after successful setup. Turn abort wins
/// over setup completion or timeout, and a pre-aborted turn never calls the
/// factory. Events with only tool-free entries skip setup entirely.
// qual:allow(complexity, max_cognitive=18) reason: "The per-hook loop folds four policies (first-resume-wins, block discards, per-hook on_error, deny wins) that belong to one chain walk; extracting them would hide the ordering contract."
// qual:allow(iosp) — chain state (blocks, rewrite, resume) accumulates per hook in declaration order; a helper per policy would re-thread the same mutable outcome.
pub async fn run_event_hooks<F>(
    config: &GlobalConfig,
    input: &EventHookInput,
    abort_signal: &AbortSignal,
    tool_context: impl FnOnce() -> F,
) -> Result<EventHookOutcome>
where
    F: std::future::Future<Output = ToolEvalContext>,
{
    let event = input.event;
    let hooks = config.read().resolved_event_hooks()?.list(event).to_vec();
    if hooks.is_empty() {
        return Ok(EventHookOutcome::default());
    }
    let tools = ToolContext::prepare(config, &hooks, abort_signal, tool_context).await;
    let mut outcome = EventHookOutcome::default();
    let mut prompt = input.prompt.clone();
    for hook in &hooks {
        let mut input_json = input.to_json();
        if let (Some(prompt), Some(object)) = (&prompt, input_json.as_object_mut()) {
            object.insert("prompt".to_string(), json!(prompt));
        }
        match run_one(hook, event, &input_json, abort_signal, &tools).await {
            Ok(state) => {
                outcome.context.extend(state.context);
                if let Some(reason) = state.block {
                    // A block makes the whole chain's context moot.
                    outcome.context.clear();
                    outcome.block = Some(reason);
                    return Ok(outcome);
                }
                if let Some(text) = state.rewrite {
                    prompt = Some(text.clone());
                    outcome.rewrite = Some(text);
                }
                if let Some(message) = state.resume {
                    outcome.resume = Some(message);
                    return Ok(outcome);
                }
            }
            Err(failure) => match hook.on_error {
                EventHookOnError::Allow => {
                    log::warn!(
                        "hook '{}': run failed, effects dropped: {failure}",
                        hook.name
                    );
                }
                EventHookOnError::Deny => {
                    return Err(anyhow!("hook '{}': {failure}", hook.name));
                }
            },
        }
    }
    Ok(outcome)
}

// qual:allow(iosp) — mirrors the middleware script runner setup so the two stay recognizably aligned; the admitted interpreter session is constructed, driven and dropped in one scope.
async fn run_one(
    hook: &ResolvedEventHook,
    event: HookEventKind,
    input_json: &Value,
    abort_signal: &AbortSignal,
    tools: &ToolContext,
) -> std::result::Result<RunState, String> {
    // Also covers abort while the shared tool context was being built.
    if abort_signal.aborted() {
        return Err(format!("hook '{}' interrupted", hook.name));
    }
    let tools = tools.for_hook(hook, event)?;
    let state = Arc::new(Mutex::new(RunState::default()));
    let (script_commands, python_functions) =
        operations::register(hook, event, input_json, state.clone(), tools);
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let script = source(&hook.source);
    // qual:allow(dry, fragment) reason: "Interpreter admission mirrors the middleware runner by design (plan D7 reuses the same runtime); a shared constructor would couple the two policy layers."
    let run = BashKitBackend.run(ScriptRequest {
        script: &script,
        limits: ScriptLimits {
            timeout_secs: hook.timeout.as_secs().saturating_add(1),
            ..Default::default()
        },
        commands: script_commands,
        python_functions: Some(python_functions),
        fs_mounts: vec![],
        cancel: cancel.clone(),
    });
    tokio::pin!(run);
    let result = tokio::select! {
        result = &mut run => result,
        () = tokio::time::sleep(hook.timeout) => {
            cancel.cancel();
            let _ = run.await;
            return Err(format!("hook '{}' timed out after {:?}", hook.name, hook.timeout));
        }
        () = wait_abort_signal(abort_signal) => {
            cancel.cancel();
            let _ = run.await;
            return Err(format!("hook '{}' interrupted", hook.name));
        }
    };
    if result.status != ScriptStatus::Ok {
        return Err(format!(
            "hook '{}' exited with {:?} (exit code {}): {}{}",
            hook.name,
            result.status,
            result.exit_code,
            result.error.unwrap_or_default(),
            result.stderr
        ));
    }
    // The registered operations keep their own clones of the Arc until the
    // script host drops them, so try_unwrap would race and silently lose
    // effects. Clone out under the lock instead.
    let state = state.lock().clone();
    Ok(state)
}

/// BashKit wraps Python sources in a heredoc so both languages share the run.
fn source(source: &harnx_core::middleware::MiddlewareSource) -> String {
    match source {
        harnx_core::middleware::MiddlewareSource::Bash(source) => source.clone(),
        harnx_core::middleware::MiddlewareSource::Python(source) => {
            let delimiter = format!("HARNX_PYTHON_{}", uuid::Uuid::new_v4().simple());
            format!("python3 <<'{delimiter}'\n{source}\n{delimiter}\n")
        }
    }
}

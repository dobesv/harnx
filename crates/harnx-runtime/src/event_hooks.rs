// qual:allow(srp, file_length=391) reason: "Input types, effect application and the shared script runner form one cohesive in-process event-hook unit; splitting it would leak partial state across modules."
//! In-process lifecycle event hooks (plan D7).
//!
//! Runs the resolved per-event hook chains from
//! `Config::resolved_event_hooks` through the same BashKit/Monty script
//! runtime the tool middleware uses. Scripts see `event_input`, may `fetch`
//! allowlisted URLs, and record effects with the event's operations:
//! `add_context` (SessionStart, UserPromptSubmit), `block`/`rewrite_prompt`
//! (UserPromptSubmit), and `resume` (Stop). Interaction operations
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

use crate::config::{GlobalConfig, ResolvedEventHook};
use anyhow::{anyhow, ensure, Result};
use harnx_core::abort::wait_abort_signal;
use harnx_core::hooks::{EventHookOnError, HookEventKind};
use harnx_script::{
    BashKitBackend, CancellationToken, ExcType, ExtFunctionResult, MontyException, MontyObject,
    ScriptBackend, ScriptCommand, ScriptCommandContext, ScriptCommandOutput, ScriptHostContext,
    ScriptLimits, ScriptPythonFunctions, ScriptPythonHandler, ScriptRequest, ScriptStatus,
};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

use crate::utils::AbortSignal;

/// Operations registered for each event. Everything else is unknown and fails
/// the run — that is how interactions stay unavailable here.
fn operations(event: HookEventKind) -> &'static [&'static str] {
    match event {
        HookEventKind::SessionStart => &["event_input", "fetch", "add_context"],
        HookEventKind::UserPromptSubmit => &[
            "event_input",
            "fetch",
            "add_context",
            "block",
            "rewrite_prompt",
        ],
        HookEventKind::Stop => &["event_input", "fetch", "resume"],
        HookEventKind::StopFailure => &["event_input", "fetch"],
    }
}

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
// qual:allow(complexity, max_cognitive=18) reason: "The per-hook loop folds four policies (first-resume-wins, block discards, per-hook on_error, deny wins) that belong to one chain walk; extracting them would hide the ordering contract."
// qual:allow(iosp) — chain state (blocks, rewrite, resume) accumulates per hook in declaration order; a helper per policy would re-thread the same mutable outcome.
pub async fn run_event_hooks(
    config: &GlobalConfig,
    input: &EventHookInput,
    abort_signal: &AbortSignal,
) -> Result<EventHookOutcome> {
    let event = input.event;
    let hooks = config.read().resolved_event_hooks()?.list(event).to_vec();
    if hooks.is_empty() {
        return Ok(EventHookOutcome::default());
    }
    let mut outcome = EventHookOutcome::default();
    let mut prompt = input.prompt.clone();
    for hook in &hooks {
        let mut input_json = input.to_json();
        if let (Some(prompt), Some(object)) = (&prompt, input_json.as_object_mut()) {
            object.insert("prompt".to_string(), json!(prompt));
        }
        match run_one(hook, event, &input_json, abort_signal).await {
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

// qual:allow(complexity, max_function_lines=73) reason: "One hook run = script setup, admission, op dispatch and error policy in one place; splitting separates each op from the error semantics it belongs to."
// qual:allow(iosp) — mirrors the middleware script runner setup so the two stay recognizably aligned; the admitted interpreter session is constructed, driven and dropped in one scope.
async fn run_one(
    hook: &ResolvedEventHook,
    event: HookEventKind,
    input_json: &Value,
    abort_signal: &AbortSignal,
) -> std::result::Result<RunState, String> {
    let state = Arc::new(Mutex::new(RunState::default()));
    let commands: Vec<Arc<Operation>> = operations(event)
        .iter()
        .map(|name| {
            Arc::new(Operation {
                name,
                state: state.clone(),
                input: input_json.clone(),
                fetch: hook.fetch.clone(),
            })
        })
        .collect();
    let script_commands: Vec<Arc<dyn ScriptCommand>> = commands
        .iter()
        .map(|operation| {
            let operation = operation.clone();
            Arc::new(BashOperation(operation)) as Arc<dyn ScriptCommand>
        })
        .collect();
    let python_functions = ScriptPythonFunctions {
        names: operations(event)
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
        handler: Arc::new(PythonOperation {
            operations: commands.clone(),
        }),
    };
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

/// One registered operation. The Bash command and Python function paths share
/// `execute_value`; only the value↔text conversion differs.
struct Operation {
    name: &'static str,
    state: Arc<Mutex<RunState>>,
    input: Value,
    fetch: Vec<String>,
}

struct BashOperation(Arc<Operation>);

#[async_trait::async_trait]
impl ScriptCommand for BashOperation {
    fn name(&self) -> &str {
        self.0.name
    }

    async fn run(
        &self,
        args: &[String],
        _stdin: Option<&[u8]>,
        ctx: ScriptCommandContext<'_>,
    ) -> std::result::Result<ScriptCommandOutput, String> {
        let host = ScriptHostContext {
            cancel: ctx.cancel,
            control: ctx.control,
        };
        match self.0.execute(args, &host).await {
            Ok(Some(value)) => Ok(ScriptCommandOutput {
                stdout: format!("{value}\n"),
                ..Default::default()
            }),
            Ok(None) => Ok(ScriptCommandOutput::default()),
            Err(error) => {
                host.control.stop();
                Err(format!("{error:#}"))
            }
        }
    }
}

impl Operation {
    async fn execute(&self, args: &[String], host: &ScriptHostContext) -> Result<Option<Value>> {
        match self.name {
            "event_input" => {
                ensure!(args.is_empty(), "event_input takes no arguments");
                Ok(Some(self.input.clone()))
            }
            "fetch" => {
                ensure!(args.len() == 1, "fetch requires one HTTP(S) URL");
                crate::middleware::fetch::fetch_url(&args[0], &self.fetch, &host.cancel)
                    .await
                    .map(Some)
            }
            "add_context" => {
                let message = args.join(" ");
                ensure!(!message.trim().is_empty(), "add_context requires a message");
                self.state.lock().context.push(message);
                Ok(None)
            }
            "block" => {
                let reason = args.join(" ");
                ensure!(!reason.trim().is_empty(), "block requires a reason");
                self.state.lock().block = Some(reason);
                Ok(None)
            }
            "rewrite_prompt" => {
                let prompt = args.join(" ");
                ensure!(
                    !prompt.trim().is_empty(),
                    "rewrite_prompt requires a prompt"
                );
                self.state.lock().rewrite = Some(prompt);
                Ok(None)
            }
            "resume" => {
                // The message is optional: an empty resume keeps the loop's
                // default continuation text.
                self.state.lock().resume = Some(args.join(" "));
                Ok(None)
            }
            _ => Err(anyhow!("unknown event-hook operation")),
        }
    }

    fn parameters(&self) -> &'static [&'static str] {
        match self.name {
            "event_input" => &[],
            "fetch" => &["url"],
            "add_context" => &["message"],
            "block" => &["reason"],
            "rewrite_prompt" => &["prompt"],
            "resume" => &["message"],
            _ => &[],
        }
    }
}

struct PythonOperation {
    operations: Vec<Arc<Operation>>,
}

#[async_trait::async_trait]
impl ScriptPythonHandler for PythonOperation {
    async fn call(
        &self,
        name: String,
        args: Vec<MontyObject>,
        kwargs: Vec<(MontyObject, MontyObject)>,
        host: ScriptHostContext,
    ) -> ExtFunctionResult {
        let result = async {
            let operation = self
                .operations
                .iter()
                .find(|operation| operation.name == name)
                .ok_or_else(|| anyhow!("unknown event-hook function '{name}'"))?;
            let values = bind(operation.parameters(), args, kwargs)?;
            let args = values.iter().map(|value| match value {
                Value::String(text) => text.clone(),
                _ => String::new(),
            });
            operation.execute(&args.collect::<Vec<_>>(), &host).await
        }
        .await;
        match result.and_then(|value| {
            value
                .map(crate::middleware::conversion::to_python)
                .transpose()
        }) {
            Ok(object) => ExtFunctionResult::Return(object.unwrap_or(MontyObject::None)),
            Err(error) => {
                host.control.stop();
                ExtFunctionResult::Error(MontyException::new(
                    ExcType::RuntimeError,
                    Some(format!("{error:#}")),
                ))
            }
        }
    }
}

/// Bind positional and keyword arguments to the operation's parameters,
/// mirroring the middleware Python bridge.
fn bind(
    parameters: &[&str],
    args: Vec<MontyObject>,
    kwargs: Vec<(MontyObject, MontyObject)>,
) -> Result<Vec<Value>> {
    ensure!(
        args.len() <= parameters.len(),
        "too many positional event-hook arguments"
    );
    let mut values = parameters
        .iter()
        .zip(args)
        .map(|(_, value)| crate::middleware::conversion::from_python(value))
        .collect::<Result<Vec<_>>>()?;
    values.resize_with(parameters.len(), || Value::Null);
    for (key, value) in kwargs {
        let name = match crate::middleware::conversion::from_python(key)? {
            Value::String(name) => name,
            _ => anyhow::bail!("event-hook keyword names must be strings"),
        };
        let index = parameters
            .iter()
            .position(|parameter| *parameter == name)
            .ok_or_else(|| anyhow!("unknown event-hook argument '{name}'"))?;
        values[index] = crate::middleware::conversion::from_python(value)?;
    }
    Ok(values)
}

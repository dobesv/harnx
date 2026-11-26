use std::{collections::BTreeMap, sync::Arc};

use anyhow::anyhow;
use harnx_core::abort::AbortSignal;
use harnx_core::script_tool::{ScriptToolDefinition, ScriptToolSource};
use harnx_core::tool::{
    ToolDeclaration, ToolError, ToolProgress, ToolProvider, ToolProviderOutput,
};
use harnx_engine::tool::ToolEvalContext;
use harnx_script::{BashKitBackend, CancellationToken, ScriptBackend, ScriptCommand, ScriptLimits};
use serde_json::Value;

use crate::bash_inner::{
    InnerCallExecutor, InnerCallExecutorConfig, InnerProgressAggregator, PayloadLimit,
    ScriptInnerExecutor,
};
use crate::bash_provider::BashEvalContextSlot;
use crate::config::{script_tools::ResolvedScriptTool, GlobalConfig};
use harnx_script::commands::{tool_commands, ToolSpec};

/// One script-tool round context slot shared with `BashProvider`: both get the
/// dispatch-scoped `ToolEvalContext` through the same weak slot, filled by the
/// runtime right after the round's context is built.
pub type ScriptEvalContextSlot = BashEvalContextSlot;

/// The agent-facing script tool provider. One instance per tool round, built
/// in `build_tool_providers` with the round's accepted view and the base
/// provider list.
pub struct ScriptToolProvider {
    /// Accepted view captured at construction: the same snapshot
    /// `tool_declarations_for_use_tools` served declarations from.
    pub(super) tools: BTreeMap<String, Arc<ResolvedScriptTool>>,
    /// Base providers only. Never contains this provider or the `bash`
    /// provider, so inner dispatch cannot recurse (decision 1).
    base_providers: Vec<Arc<dyn ToolProvider>>,
    /// Inner `tools:` selectors resolve against current base-tool discovery
    /// at call time (decision 5), so the config must stay reachable.
    config: GlobalConfig,
    slot: Option<Arc<ScriptEvalContextSlot>>,
    payload_limit: Option<PayloadLimit>,
    backend: BashKitBackend,
    attachment_fetcher: Option<Arc<dyn harnx_script::fs_attachments::AttachmentFetcher>>,
    media_store: Option<Arc<dyn crate::bash_inner::media::MediaStore>>,
    /// Script-facing declarations of tool servers, by agent-visible tool
    /// name. Only Bash bodies read them; see `inner_run`.
    script_shapes: BTreeMap<String, crate::ScriptShape>,
}

/// Inputs resolved before a round's evaluation context exists.
pub(crate) struct ScriptToolProviderConfig {
    /// The accepted view (`Config::accepted_script_tools`).
    pub tools: Vec<Arc<ResolvedScriptTool>>,
    /// Base providers, excluding the `bash` provider and this provider.
    pub base_providers: Vec<Arc<dyn ToolProvider>>,
    pub config: GlobalConfig,
    pub slot: Option<Arc<ScriptEvalContextSlot>>,
    pub payload_limit: Option<PayloadLimit>,
    pub attachment_fetcher: Option<Arc<dyn harnx_script::fs_attachments::AttachmentFetcher>>,
    pub media_store: Option<Arc<dyn crate::bash_inner::media::MediaStore>>,
    /// Script-facing declarations of tool servers, by agent-visible tool name.
    pub script_shapes: BTreeMap<String, crate::ScriptShape>,
}

pub(super) struct ScriptToolInvocation<'a> {
    pub tool_round: Option<u64>,
    pub abort: &'a AbortSignal,
    pub progress: Arc<dyn ToolProgress>,
}

/// The runnable body of a definition. Both languages run through the same
/// BashKit backend: a Python body becomes a `python3` heredoc script, the
/// same shape the event-hook runner uses, with the `script_tool_provider`
/// Python host functions attached.
enum ScriptBody {
    Bash(String),
    Python(String),
}

fn script_body(definition: &ScriptToolDefinition) -> ScriptBody {
    match &definition.source {
        ScriptToolSource::Bash { bash } => ScriptBody::Bash(bash.clone()),
        ScriptToolSource::Python { python } => ScriptBody::Python(python.clone()),
    }
}

/// Wrap a Monty Python body in a `python3` heredoc. Fresh delimiter per run so
/// a body cannot close the heredoc early.
fn python_script(body: &str) -> String {
    let delimiter = format!("HARNX_PYTHON_{}", uuid::Uuid::new_v4().simple());
    format!("python3 <<'{delimiter}'\n{body}\n{delimiter}\n")
}

impl ScriptToolProvider {
    pub(crate) fn new(config: ScriptToolProviderConfig) -> Self {
        let ScriptToolProviderConfig {
            tools,
            base_providers,
            config,
            slot,
            payload_limit,
            attachment_fetcher,
            media_store,
            script_shapes,
        } = config;
        Self {
            tools: tools
                .into_iter()
                .map(|tool| (tool.definition().name.clone(), tool))
                .collect(),
            base_providers,
            config,
            slot,
            payload_limit,
            backend: BashKitBackend,
            attachment_fetcher,
            media_store,
            script_shapes,
        }
    }

    /// Assemble the BashKit command set for one Bash run: the CLI commands
    /// for the resolved inner allowlist plus `tool_input`. Pure assembly
    /// around the reused `bash` script-mode machinery; no script logic lives
    /// here. Python runs pass no CLI commands — their `tool_input` and
    /// `tools` are Python host functions over the same run.
    fn bash_commands(
        &self,
        definition: &ScriptToolDefinition,
        declarations: &[ToolDeclaration],
        run: Arc<crate::bash_inner::InnerCallRun>,
        arguments: Value,
        invocation: &ScriptToolInvocation<'_>,
    ) -> Result<Vec<Arc<dyn ScriptCommand>>, ToolError> {
        let adapter = Arc::new(ScriptInnerExecutor::new(run, invocation.abort.clone()));
        let specs = declarations
            .iter()
            .map(|declaration| ToolSpec {
                name: declaration.name.clone(),
                description: declaration.description.clone(),
                schema: declaration.parameters.as_value().clone(),
                // The run asks for structured results, so a tool's declared
                // output mapping decides what its command prints.
                output: self
                    .script_shapes
                    .get(&declaration.name)
                    .and_then(|shape| shape.output.as_ref())
                    .and_then(crate::bash_provider::output_mapping),
                output_schema: self
                    .script_shapes
                    .get(&declaration.name)
                    .and_then(|shape| shape.output_schema.clone()),
            })
            .collect();
        let mut commands = tool_commands(specs, adapter).map_err(|error| {
            ToolError::Recoverable(anyhow!(
                "script tool '{}' failed to register its tool commands: {error}",
                definition.name
            ))
        })?;
        commands.push(Arc::new(super::tool_input::ToolInputCommand {
            input: arguments,
        }));
        Ok(commands)
    }

    fn context_slot(&self, name: &str) -> Result<Arc<ToolEvalContext>, ToolError> {
        self.slot
            .as_ref()
            .and_then(|slot| slot.get())
            .ok_or_else(|| {
                ToolError::Recoverable(anyhow!(
                    "script tool '{name}' cannot run in this execution context: no tool evaluation context available"
                ))
            })
    }

    /// Decision 5: resolve the allowlist against what the base providers
    /// expose right now, excluding control tools and every script tool.
    fn inner_declarations(
        &self,
        definition: &ScriptToolDefinition,
        context: &ToolEvalContext,
    ) -> Vec<ToolDeclaration> {
        let config = self.config.read();
        config.script_tool_inner_declarations(
            &definition.tools,
            context.current_agent_package.as_deref(),
        )
    }

    /// The `InnerCallExecutor` run for one script-tool body: the same
    /// forked-context traversal, budgets and pre-flight the `bash` provider
    /// uses, so decision 11's parity holds by construction.
    ///
    /// A Bash body asks its inner calls for structured results because its
    /// commands read each tool's output mapping. A Python body's `tools` calls
    /// return the model-style result, so it keeps the plain results.
    // qual:allow(iosp) — one optional-config branch over reused bash executor constructors; the branch is the whole logic and cannot live apart from the calls it configures
    fn inner_run(
        &self,
        definition: &ScriptToolDefinition,
        declarations: &[ToolDeclaration],
        context: &ToolEvalContext,
        invocation: &ScriptToolInvocation<'_>,
    ) -> Arc<crate::bash_inner::InnerCallRun> {
        let allowed = declarations
            .iter()
            .map(|declaration| declaration.name.clone())
            .collect();
        let mut executor_config =
            InnerCallExecutorConfig::new(allowed, self.base_providers.clone());
        executor_config.structured_results =
            matches!(definition.source, ScriptToolSource::Bash { .. });
        if let Some(payload_limit) = &self.payload_limit {
            executor_config = executor_config.with_payload_limit(payload_limit.clone());
        }
        let executor = InnerCallExecutor::new(executor_config);
        let aggregator = Arc::new(InnerProgressAggregator::new(invocation.progress.clone()));
        Arc::new(
            executor
                .begin_run(context, invocation.tool_round, Some(aggregator))
                .with_media(self.media_store.clone()),
        )
    }

    /// Run the definition's body over the shared inner run (decision 11):
    /// Bash dispatches its allowlisted tools as CLI commands; Python uses
    /// host functions over the same run. Returns the backend run plus the
    /// reason a Python body's terminal fatal inner call recorded before
    /// stopping the host (4659980781); a Bash body reports its fatal
    /// through `ScriptRun.error`, so its reason is `None`.
    async fn run_body(
        &self,
        definition: &harnx_core::script_tool::ScriptToolDefinition,
        declarations: &[ToolDeclaration],
        run: Arc<crate::bash_inner::InnerCallRun>,
        arguments: Value,
        invocation: &ScriptToolInvocation<'_>,
    ) -> Result<(harnx_script::ScriptRun, Option<String>), ToolError> {
        let timeout_secs = definition.timeout.as_secs().max(1);
        match script_body(definition) {
            ScriptBody::Bash(script) => {
                let commands =
                    self.bash_commands(definition, declarations, run, arguments, invocation)?;
                Ok((
                    self.run_backend(&script, timeout_secs, commands, None, invocation.abort)
                        .await,
                    None,
                ))
            }
            ScriptBody::Python(body) => {
                let script = python_script(&body);
                Ok(self
                    .run_python_body(
                        &script,
                        timeout_secs,
                        run,
                        (arguments, declarations),
                        invocation,
                    )
                    .await)
            }
        }
    }

    /// Run a Python body: `tool_input` and `tools` are host functions over
    /// the shared run, with no CLI command set. Returns the run plus the
    /// reason a terminal fatal inner call recorded before stopping the host
    /// (4659980781), which the outcome reports on the `Stopped` status.
    async fn run_python_body(
        &self,
        script: &str,
        timeout_secs: u64,
        run: Arc<crate::bash_inner::InnerCallRun>,
        (arguments, declarations): (Value, &[ToolDeclaration]),
        invocation: &ScriptToolInvocation<'_>,
    ) -> (harnx_script::ScriptRun, Option<String>) {
        let fatal = std::sync::Arc::new(std::sync::OnceLock::new());
        let functions = super::python::functions(
            run,
            invocation.abort.clone(),
            arguments,
            declarations,
            fatal.clone(),
        );
        let result = self
            .run_backend(
                script,
                timeout_secs,
                Vec::new(),
                Some(functions),
                invocation.abort,
            )
            .await;
        (result, fatal.get().cloned())
    }

    /// Same abort bridge as `BashProvider::run_script`: cancel the token and
    /// still await the backend, so pending inner calls are dropped (their
    /// futures observe cancellation) instead of leaking.
    async fn run_backend(
        &self,
        script: &str,
        timeout_secs: u64,
        commands: Vec<Arc<dyn ScriptCommand>>,
        python_functions: Option<harnx_script::ScriptPythonFunctions>,
        abort: &AbortSignal,
    ) -> harnx_script::ScriptRun {
        let limits = ScriptLimits {
            timeout_secs,
            ..ScriptLimits::default()
        };
        let token = CancellationToken::new();
        let run_fut = self.backend.run(harnx_script::ScriptRequest {
            script,
            python_functions,
            limits,
            commands,
            fs_mounts: vec![crate::bash_provider::attachments::mount(
                self.attachment_fetcher.clone(),
            )],
            cancel: token.clone(),
        });
        tokio::pin!(run_fut);
        tokio::select! {
            result = &mut run_fut => result,
            _ = harnx_core::abort::wait_abort_signal(abort) => {
                token.cancel();
                (&mut run_fut).await
            }
        }
    }

    pub(super) async fn run_script_tool(
        &self,
        tool: &Arc<ResolvedScriptTool>,
        arguments: Value,
        invocation: ScriptToolInvocation<'_>,
    ) -> Result<ToolProviderOutput, ToolError> {
        let definition = tool.definition();
        // R8: validate against the definition's JSON Schema before anything
        // runs. The script never executes on invalid arguments.
        tool.validate_arguments(&arguments)
            .map_err(|error| ToolError::Recoverable(anyhow!("{error:#}")))?;
        let context = self.context_slot(&definition.name)?;
        // The backend timeout is whole seconds (C11): a sub-second timeout is
        // truncated to zero and raised to the one-second floor.
        let timeout_secs = definition.timeout.as_secs().max(1);
        // One inner run per body: the same allowlist resolution, forked
        // context, budgets and pre-flight for both languages (decision 11).
        let declarations = self.inner_declarations(definition, &context);
        let run = self.inner_run(definition, &declarations, &context, &invocation);
        let (result, host_stop) = self
            .run_body(
                definition,
                &declarations,
                run.clone(),
                arguments,
                &invocation,
            )
            .await?;
        // Capture the trace after the body finished, so failure payloads
        // report the inner calls like the `bash` tool does.
        let trace = super::outcome::ScriptToolTrace {
            entries: run.trace(),
            dropped: run.dropped_trace_entries(),
            first_failed: run.first_failed(),
        };
        super::outcome::ScriptToolOutcome {
            name: definition.name.clone(),
            output: definition.output,
            timeout_secs,
            result,
            aborted: invocation.abort.aborted(),
            trace,
            host_stop,
        }
        .into_provider_result()
    }
}

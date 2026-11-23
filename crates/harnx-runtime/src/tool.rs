use crate::{
    config::{Config, GlobalConfig},
    utils::*,
};
use anyhow::Result;

use serde_json::Value;
use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;
use std::sync::Arc;

use harnx_core::tool::ToolProvider;
pub use harnx_core::tool::{
    extract_user_display_text, render_tool_call_template, render_tool_result_template, JsonSchema,
    SwitchAgentData, ToolCall, ToolDeclaration, ToolResult, ToolUpdatePatch, Tools,
};
use harnx_engine::tool::ToolEvalRenderContext;
pub use harnx_engine::tool::{
    eval_tool_calls, ConfirmToolUseFn, PrepareHandoffFn, PreparedHandoff, ToolCallEmitFn,
    ToolEvalContext, ToolUpdateEmitFn, ToolUseConfirmation,
};

/// The LLM text completion that immediately preceded a tool round.
/// Groups the assistant output text and optional chain-of-thought together
/// so `execute_tool_round` doesn't need separate `output` and `thought` args.
pub struct CompletionText<'a> {
    pub output: &'a str,
    pub thought: Option<&'a str>,
}

use crate::nats_tool_provider::NatsToolProvider;
use crate::tool_context::discover_nats_tool_provider_cached;
pub use crate::tool_context::{BuildToolEvalContextParams, ToolRoundParams};

/// Whether a tool round appends its `ToolCalls` entry or resumes one the
/// session log already holds.
#[derive(Debug, Clone)]
pub enum ToolRoundPersistence {
    /// Append the round's `ToolCalls` entry before dispatching its calls.
    AppendCalls,
    /// The log already holds the round's `ToolCalls` entry, at `seq`.
    ExistingCalls { seq: u64 },
    /// Old process-hook decisions are applied without invoking the retired hooks.
    LegacyApprovals { seq: u64, denied: Vec<ToolResult> },
}

impl ToolRoundPersistence {
    fn denied_results(self) -> Vec<ToolResult> {
        match self {
            Self::LegacyApprovals { denied, .. } => denied,
            _ => Vec::new(),
        }
    }

    /// Open the round `calls` belong to and return its sequence, the one their
    /// `ToolCalls` entry has. A new round appends that entry here, before any
    /// call is dispatched. `None` when no durable entry holds the round, as in
    /// a dry run.
    fn open_round(&self, params: &ToolRoundParams<'_>, calls: &[ToolCall]) -> Result<Option<u64>> {
        match self {
            Self::ExistingCalls { seq } | Self::LegacyApprovals { seq, .. } => Ok(Some(*seq)),
            Self::AppendCalls if params.config.read().dry_run => Ok(None),
            Self::AppendCalls => params.config.write().append_session_tool_calls(
                params.input,
                params.completion.output,
                params.completion.thought,
                calls,
            ),
        }
    }
}

/// Persist a tool round and execute its calls.  Writes the
/// `ToolCalls` session-log entry BEFORE running tools (so the
/// transcript captures the request even on crash/interrupt), runs
/// `eval_tool_calls`, then writes the matching `ToolResults` entry.
///
/// On eval failure, synthesizes one error-output `ToolResult` per
/// call, writes those to keep the log well-formed, and returns the
/// original error.  Skips both writes entirely when `dry_run` is set.
pub async fn execute_tool_round(
    params: ToolRoundParams<'_>,
    tool_calls: Vec<ToolCall>,
) -> Result<Vec<ToolResult>> {
    // Keep checkpoint/resume state out of the model loop's future. Inlining
    // this frame overflows bounded worker stacks during nested tool dispatch.
    Box::pin(execute_tool_round_with_persistence(
        params,
        tool_calls,
        ToolRoundPersistence::AppendCalls,
    ))
    .await
}

pub async fn execute_tool_round_with_persistence(
    params: ToolRoundParams<'_>,
    mut tool_calls: Vec<ToolCall>,
    persistence: ToolRoundPersistence,
) -> Result<Vec<ToolResult>> {
    anyhow::ensure!(
        !params.abort_signal.aborted(),
        "interrupted during tool execution"
    );
    harnx_engine::tool::ensure_tool_call_ids(&mut tool_calls);
    let tool_round = persistence.open_round(&params, &tool_calls)?;
    let legacy_denied = persistence.denied_results();
    let config = params.config;
    let input = params.input;
    let abort_signal = params.abort_signal;
    let dry_run = config.read().dry_run;
    // A call on a tool that declared `harnx:input_required` must find the
    // durable interaction path open even when no middleware is configured;
    // an undeclared tool's challenge degrades to an error result instead.
    let input_declared = round_may_request_input(
        config,
        params.instance_id,
        params.input.agent().name(),
        &tool_calls,
    )
    .await;
    let durable = crate::interaction_checkpoint::live::DurableRound::open(
        config,
        tool_round,
        &tool_calls,
        input_declared,
    )
    .await?;
    let eval_ctx = build_round_eval_context(&params, tool_round, durable.as_ref()).await;
    let tool_loop_guard = params.tool_loop_guard;
    let guard = crate::tool_loop_guard::begin_round(tool_loop_guard.as_deref(), config, input);
    let (to_eval, refused) =
        screen_round_with_saved_denials(guard, config, &tool_calls, &eval_ctx, legacy_denied)
            .await?;
    let results = evaluate_round(
        ToolFailure {
            config,
            calls: &tool_calls,
            eval: &eval_ctx,
            abort: abort_signal,
        },
        to_eval,
        &refused,
        durable.as_ref(),
    )
    .await?;
    anyhow::ensure!(!abort_signal.aborted(), "interrupted during tool execution");
    let results = crate::tool_loop_guard::finish_round(guard, results, refused, &tool_calls);
    let mut results = populate_result_markdown(results, &eval_ctx);
    persist_tool_results(config, &mut results, dry_run, durable.as_ref()).await?;
    Ok(results)
}

/// Whether any call in the round targets a NATS tool that declared
/// `harnx:input_required` (`ToolSpec::with_input_required`), in which case
/// the durable interaction path must be open even without middleware.
/// Discovery is cached, so the round's later context build reuses it.
async fn round_may_request_input(
    config: &GlobalConfig,
    instance_id: &harnx_core::instance::ServerScope,
    agent_name: &str,
    calls: &[ToolCall],
) -> bool {
    let Some(provider) = round_nats_provider(config, instance_id, agent_name, calls).await else {
        return false;
    };
    calls
        .iter()
        .any(|call| provider.may_request_input(&call.name))
}

/// The round's cached NATS provider when the round may consult one. A round
/// with no calls has nothing to look up, and a process without a broker
/// address must not start one just to check for input-capable tools.
// Type-erased box, not a plain `Box::pin`: auto-trait evaluation still
// descends through `Pin<Box<impl Future>>`, and discovery reaches the deep
// async-nats connect chain, so a concrete box here overflows dependent
// crates' trait-solver recursion budget. Erasing the future type cuts the
// chain.
fn round_nats_provider<'a>(
    config: &'a GlobalConfig,
    instance_id: &'a harnx_core::instance::ServerScope,
    agent_name: &'a str,
    calls: &'a [ToolCall],
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Arc<NatsToolProvider>>> + Send + 'a>>
{
    Box::pin(async move {
        match round_can_consult_nats_tools(calls, crate::tool_context::has_broker_address()) {
            true => {
                let package = harnx_core::package_namespace::pkg_from_qualified(agent_name)
                    .map(str::to_string);
                let config = config.read().clone();
                discover_nats_tool_provider_cached(&config, instance_id, package.as_deref()).await
            }
            false => None,
        }
    })
}

/// A round with no calls has nothing to look up, and a process without a
/// broker address must not start one just to check for input-capable tools.
fn round_can_consult_nats_tools(calls: &[ToolCall], has_broker: bool) -> bool {
    !calls.is_empty() && has_broker
}

async fn screen_round_with_saved_denials(
    guard: Option<&parking_lot::Mutex<harnx_core::loop_guard::ToolRepeatGuard>>,
    config: &GlobalConfig,
    calls: &[ToolCall],
    eval: &ToolEvalContext,
    mut denied: Vec<ToolResult>,
) -> Result<(Vec<ToolCall>, Vec<ToolResult>)> {
    for result in &denied {
        (eval.emit_tool_blocked_fn)(&result.call, &result.output);
    }
    let candidates: Vec<_> = calls
        .iter()
        .filter(|call| !denied.iter().any(|result| result.call == **call))
        .cloned()
        .collect();
    let (to_eval, mut refused) = screen_tool_round(guard, config, &candidates, eval).await?;
    refused.append(&mut denied);
    Ok((to_eval, refused))
}

async fn build_round_eval_context(
    params: &ToolRoundParams<'_>,
    tool_round: Option<u64>,
    durable: Option<&crate::interaction_checkpoint::live::DurableRound>,
) -> Arc<ToolEvalContext> {
    let agent_use_tools = params.input.agent().use_tools().map(|v| v.join(","));
    // Bare handoff targets resolve to the active agent's package (#709).
    let current_agent_package =
        harnx_core::package_namespace::pkg_from_qualified(params.input.agent().name())
            .map(str::to_string);
    // The slot holds a Weak, so installing the completed context cannot form
    // a provider/context cycle. Set the round before the Arc makes it immutable.
    let slot = crate::bash_provider::BashEvalContextSlot::new();
    let mut ctx = build_tool_eval_context(BuildToolEvalContextParams {
        config: params.config,
        instance_id: params.instance_id,
        agent_use_tools: agent_use_tools.as_deref(),
        current_agent_package,
        working_dir: params.working_dir,
        prepare_handoff_fn: params.prepare_handoff_fn.clone(),
        pending_async_context: params.pending_async_context.clone(),
        bash_context_slot: Some(slot.clone()),
        command_bash: None,
        extra_base_providers: Vec::new(),
    })
    .await;
    ctx.tool_round = tool_round;
    if let Some(round) = durable {
        round.install(&mut ctx);
    }
    slot.install(ctx)
}

async fn persist_tool_results(
    config: &GlobalConfig,
    results: &mut [ToolResult],
    dry_run: bool,
    durable: Option<&crate::interaction_checkpoint::live::DurableRound>,
) -> Result<()> {
    if dry_run {
        return Ok(());
    }
    let persistence = {
        config
            .write()
            .prepare_session_tool_results_in_place(results)?
    };
    let session_id = durable
        .map(|d| d.session.backend.session_id().to_string())
        .or_else(|| config.read().session.as_ref().map(|s| s.id.clone()))
        .unwrap_or_default();
    crate::fault_injection::hit(
        &session_id,
        crate::fault_injection::RecoveryFaultPoint::FinalToolResultsBefore,
    )
    .await;
    persistence.persist().await;
    crate::fault_injection::hit(
        &session_id,
        crate::fault_injection::RecoveryFaultPoint::FinalToolResultsAfter,
    )
    .await;
    if let Some(round) = durable {
        round.cleanup().await?;
    }
    Ok(())
}

struct ToolFailure<'a> {
    config: &'a GlobalConfig,
    calls: &'a [ToolCall],
    eval: &'a ToolEvalContext,
    abort: &'a harnx_core::abort::AbortSignal,
}

async fn evaluate_round(
    failure: ToolFailure<'_>,
    calls: Vec<ToolCall>,
    refused: &[ToolResult],
    durable: Option<&crate::interaction_checkpoint::live::DurableRound>,
) -> Result<Vec<ToolResult>> {
    let evaluation = match durable {
        Some(round) => {
            round.complete_refused(refused, failure.eval).await?;
            round.evaluate(failure.eval, calls, failure.abort).await
        }
        None => eval_tool_calls(failure.eval, calls, failure.abort).await,
    };
    match evaluation {
        Ok(results) => Ok(results),
        // Include refused calls too when materializing an actual round failure.
        Err(error) => fail_tool_round(failure, error).await,
    }
}

async fn fail_tool_round(
    failure: ToolFailure<'_>,
    error: anyhow::Error,
) -> Result<Vec<ToolResult>> {
    if terminal_tool_interruption(failure.abort, &error) {
        return Err(error);
    }
    anyhow::ensure!(
        !failure.abort.aborted(),
        "interrupted during tool execution"
    );
    if !failure.config.read().dry_run {
        persist_failed_tool_results(failure.config, failure.calls.to_vec(), failure.eval, &error)
            .await?;
    }
    Err(error)
}

fn terminal_tool_interruption(
    abort: &harnx_core::abort::AbortSignal,
    error: &anyhow::Error,
) -> bool {
    abort.aborted() || error.is::<harnx_engine::tool::ToolInputRequiredError>()
}

async fn persist_failed_tool_results(
    config: &GlobalConfig,
    tool_calls: Vec<ToolCall>,
    eval_ctx: &ToolEvalContext,
    error: &anyhow::Error,
) -> Result<()> {
    let fallback = tool_calls
        .into_iter()
        .map(|call| {
            ToolResult::new(
                call,
                serde_json::json!({
                    "error": format!("tool execution failed: {error:#}")
                }),
            )
        })
        .collect();
    let fallback = populate_result_markdown(fallback, eval_ctx);
    let persistence = {
        config
            .write()
            .prepare_session_tool_results(&fallback)
            .map_err(|persist_error| {
                anyhow::anyhow!(
                    "{error:#}; additionally failed to persist fallback tool results: {persist_error:#}"
                )
            })?
    };
    persistence.persist().await;
    Ok(())
}

/// Decide which of the round's calls run, or end the turn for repetition.
async fn screen_tool_round(
    guard: Option<&parking_lot::Mutex<harnx_core::loop_guard::ToolRepeatGuard>>,
    config: &GlobalConfig,
    calls: &[ToolCall],
    eval_ctx: &ToolEvalContext,
) -> Result<(Vec<ToolCall>, Vec<ToolResult>)> {
    use crate::tool_loop_guard::Screened;
    match crate::tool_loop_guard::screen_round(guard, calls, eval_ctx) {
        Screened::Proceed { to_eval, refused } => Ok((to_eval, refused)),
        Screened::Stop(terminal) => {
            let persisted = if config.read().dry_run {
                Ok(())
            } else {
                persist_stopped_tool_results(config, calls, eval_ctx, &terminal).await
            };
            Err(repetition_stop_error(terminal, persisted))
        }
    }
}

/// The error that ends a turn stopped for repetition. Parents read the stop
/// from the end of the persisted error text, so a failure to persist the
/// stopped results is added as context around the stop instead of replacing
/// it.
fn repetition_stop_error(
    terminal: harnx_core::loop_guard::RepetitionTerminal,
    persisted: Result<()>,
) -> anyhow::Error {
    let stop = anyhow::Error::new(harnx_core::loop_guard::RepetitionStop(terminal));
    match persisted {
        Ok(()) => stop,
        Err(error) => stop.context(format!("failed to persist stopped tool results: {error:#}")),
    }
}

/// None of a stopped round's calls run, but each gets a result saying why, so
/// the transcript keeps one result per call.
async fn persist_stopped_tool_results(
    config: &GlobalConfig,
    tool_calls: &[ToolCall],
    eval_ctx: &ToolEvalContext,
    terminal: &harnx_core::loop_guard::RepetitionTerminal,
) -> Result<()> {
    let output = crate::tool_loop_guard::stopped_output(terminal);
    let results = tool_calls
        .iter()
        .map(|call| ToolResult::new(call.clone(), output.clone()))
        .collect();
    let results = populate_result_markdown(results, eval_ctx);
    let persistence = config.write().prepare_session_tool_results(&results)?;
    persistence.persist().await;
    Ok(())
}

/// Build tool lifecycle emit closures over a shared declaration map.
fn build_emit_fns(
    decl_map: &Arc<HashMap<String, ToolDeclaration>>,
) -> (
    Arc<ToolCallEmitFn>,
    Arc<ToolCallEmitFn>,
    Arc<ToolCallEmitFn>,
    Arc<ToolUpdateEmitFn>,
) {
    let m1 = Arc::clone(decl_map);
    let emit_tool_call_fn: Arc<ToolCallEmitFn> =
        Arc::new(move |call: &ToolCall, json_data: &Value| {
            emit_tool_call_with_template(call, json_data, &m1);
        });
    let m2 = Arc::clone(decl_map);
    let emit_tool_result_fn: Arc<ToolCallEmitFn> =
        Arc::new(move |call: &ToolCall, result: &Value| {
            emit_tool_result_with_template(call, result, &m2);
        });
    let m3 = Arc::clone(decl_map);
    let emit_tool_blocked_fn: Arc<ToolCallEmitFn> =
        Arc::new(move |call: &ToolCall, blocked_result: &Value| {
            emit_tool_blocked_with_template(call, blocked_result, &m3);
        });
    // Capture the turn-scoped sink now. Provider implementations may call the
    // progress handle from `tokio::spawn`, which doesn't inherit task locals.
    let update_sink = harnx_core::sink::current_agent_event_sink();
    let emit_tool_update_fn: Arc<ToolUpdateEmitFn> =
        Arc::new(move |call_id: &str, patch: &ToolUpdatePatch| {
            emit_tool_update(call_id, patch, update_sink.as_deref());
        });
    (
        emit_tool_call_fn,
        emit_tool_result_fn,
        emit_tool_blocked_fn,
        emit_tool_update_fn,
    )
}

/// Emit a tool update event for live progress reporting.
/// Called by the runtime when a tool calls `progress.update(patch)`.
fn emit_tool_update(
    call_id: &str,
    patch: &ToolUpdatePatch,
    sink: Option<&dyn harnx_core::event::AgentEventSink>,
) {
    use harnx_core::event::{AgentEvent, ToolEvent};

    let event = AgentEvent::Tool(ToolEvent::Update {
        id: call_id.to_string(),
        markdown: patch.markdown.clone(),
        status: patch.status,
        content: patch.content.clone(),
        title: patch.title.clone(),
        kind: patch.kind,
        locations: patch.locations.clone(),
        usage: patch.usage.clone(),
    });

    if let Some(sink) = sink {
        sink.emit(event);
    } else {
        let _ = harnx_core::sink::emit_agent_event(event);
    }
}

async fn resolve_nats_provider(
    config: &Config,
    instance_id: &harnx_core::instance::ServerScope,
    active_package: Option<&str>,
    injected: Option<Arc<NatsToolProvider>>,
) -> Option<Arc<NatsToolProvider>> {
    match should_discover_nats_provider(
        injected.is_some(),
        crate::tool_context::has_broker_address(),
    ) {
        true => discover_nats_tool_provider_cached(config, instance_id, active_package).await,
        false => injected,
    }
}

/// In-process tests without a broker must not start the developer's broker,
/// and an explicitly injected provider wins over discovery.
fn should_discover_nats_provider(injected: bool, has_broker: bool) -> bool {
    !injected && has_broker
}

/// Build tool providers, middleware, and rendering state for one tool round.
pub async fn build_tool_eval_context(params: BuildToolEvalContextParams<'_>) -> ToolEvalContext {
    Box::pin(build_tool_eval_context_inner(params, None)).await
}

/// Build against an admitted discovery snapshot, without consulting the cache.
pub async fn build_tool_eval_context_with_provider(
    params: BuildToolEvalContextParams<'_>,
    provider: Arc<crate::nats_tool_provider::NatsToolProvider>,
) -> ToolEvalContext {
    Box::pin(build_tool_eval_context_inner(params, Some(provider))).await
}

async fn build_tool_eval_context_inner(
    params: BuildToolEvalContextParams<'_>,
    provider: Option<Arc<NatsToolProvider>>,
) -> ToolEvalContext {
    // N1: without a round context slot, script tool bodies could never run, so
    // don't advertise them to contexts without one (lifecycle hooks, operator
    // `.call`, non-Bash reservation calls, orphan repair). Operator and reserved
    // Bash contexts install a slot.
    let script_tools_enabled = params.bash_context_slot.is_some();
    // 4659980786: resolve the round's NATS provider before snapshotting, so
    // the accepted view can exclude every name the provider actually serves;
    // resolving after the snapshot would leave the view blind to tools only
    // the provider knows about.
    let config = params.config.read().clone();
    let provider = resolve_nats_provider(
        &config,
        params.instance_id,
        params.current_agent_package.as_deref(),
        provider,
    )
    .await;
    let snapshot = snapshot_tool_round(
        &config,
        provider.as_deref(),
        params.agent_use_tools,
        params.current_agent_package.as_deref(),
        script_tools_enabled,
    );
    assemble_tool_eval_context(params, snapshot, provider)
}

fn assemble_tool_eval_context(
    params: BuildToolEvalContextParams<'_>,
    snapshot: ToolRoundSnapshot,
    provider: Option<Arc<NatsToolProvider>>,
) -> ToolEvalContext {
    let decl_map = Arc::new(build_context_decl_map(
        snapshot.declarations,
        &snapshot.provider_declarations,
        params.command_bash.as_ref(),
    ));
    let (emit_tool_call_fn, emit_tool_result_fn, emit_tool_blocked_fn, emit_tool_update_fn) =
        build_emit_fns(&decl_map);
    let mut ctx = ToolEvalContext {
        structured_results: false,
        on_tool_call_completed: None,
        on_interaction: None,
        middleware_fn: None,
        work_boundary: None,
        instance_id: params.instance_id.clone(),
        tool_round: None,
        render: Some(ToolEvalRenderContext {
            decl_map: Arc::clone(&decl_map),
        }),
        allowed_tool_names: decl_map.keys().cloned().collect(),
        providers: build_tool_providers(BuildToolProvidersParams {
            config: params.config,
            nats_provider: provider,
            bash_context_slot: params.bash_context_slot,
            command_bash: params.command_bash,
            extra_base_providers: params.extra_base_providers,
            script_tools: snapshot.script_tools,
            nats_declarations: snapshot.nats_declarations,
        }),
        current_agent_package: params.current_agent_package,
        handoff_targets: snapshot.handoff_targets,
        emit_tool_call_fn,
        emit_tool_result_fn,
        emit_tool_blocked_fn,
        emit_tool_update_fn,
        confirm_tool_use_fn: snapshot.confirmation,
        prepare_handoff_fn: params.prepare_handoff_fn,
    };
    crate::middleware::install(&mut ctx, &snapshot.config);
    ctx
}

struct ToolRoundSnapshot {
    declarations: Vec<ToolDeclaration>,
    handoff_targets: HashMap<String, String>,
    confirmation: Arc<ConfirmToolUseFn>,
    config: Config,
    /// The round's accepted script-tool view (decision 12): computed once from
    /// the same captured base-declaration set as the declarations and threaded
    /// to the script provider and the bash inner catalog, so dispatch serves
    /// exactly what the round advertised even if discovery refreshes in
    /// between. Empty when the round has no script evaluation slot.
    script_tools: Vec<Arc<crate::config::script_tools::ResolvedScriptTool>>,
    /// The round provider's declarations for the agent's `use_tools`, merged
    /// into the advertised declaration map after the config declarations.
    /// Captured once with the view, so the map and the dispatch cannot
    /// disagree across a discovery refresh.
    provider_declarations: Vec<ToolDeclaration>,
    /// The config's NATS declarations at round start, the same capture the
    /// accepted view and the declarations were built from. The bash inner
    /// catalog derives from it too.
    nats_declarations: Vec<ToolDeclaration>,
}

/// Snapshot one tool round. `script_tools_enabled` is false for contexts
/// without a round context slot; those rounds carry no script tools at all —
/// neither declarations nor a dispatchable provider view.
fn snapshot_tool_round(
    config: &Config,
    provider: Option<&NatsToolProvider>,
    selectors: Option<&str>,
    package: Option<&str>,
    script_tools_enabled: bool,
) -> ToolRoundSnapshot {
    // One captured set of base declarations for the whole round (decision 12,
    // 4659980770/4659980786): the accepted script-tool view, the advertised
    // declarations, the declaration map, and the bash inner catalog all
    // derive from `nats_declarations` plus the resolved provider's
    // declarations, so a discovery refresh between two reads cannot make the
    // round advertise one tool and dispatch another under the same name.
    // The view is additionally filtered by the provider's `has_tool`, which
    // covers names the declarations cannot carry: raw aliases and names the
    // agent's `use_tools` does not select are served but never declared, and
    // the script provider precedes the NATS provider in dispatch order.
    let nats_declarations = config.nats_tool_declarations_snapshot();
    let provider_declarations = match provider {
        Some(provider) => provider.declarations_for_use_tools(selectors),
        None => Vec::new(),
    };
    let script_tools = if script_tools_enabled {
        let base_names = config.base_tool_names_with(&nats_declarations, &provider_declarations);
        exclude_provider_served(config.accepted_script_tools_against(&base_names), provider)
    } else {
        Vec::new()
    };
    let (declarations, handoff_targets) = config.tool_declarations_for_use_tools_with_script_tools(
        selectors,
        package,
        &nats_declarations,
        &script_tools,
    );
    ToolRoundSnapshot {
        declarations,
        handoff_targets,
        confirmation: build_confirm_tool_use_fn(config),
        config: config.clone(),
        script_tools,
        provider_declarations,
        nats_declarations,
    }
}

/// The accepted view minus every script tool whose name the round's
/// resolved provider *serves* (`has_tool`). The provider's declarations —
/// the names the config-level exclusion sees — are `use_tools`-filtered
/// and never contain raw aliases, but dispatch routes both, and the script
/// provider precedes the NATS provider in the round's provider order: a
/// script tool with either kind of name would shadow a live tool. Same
/// single snapshot: `provider` is the round's resolved provider, captured
/// with the declarations above.
fn exclude_provider_served(
    view: Vec<Arc<crate::config::script_tools::ResolvedScriptTool>>,
    provider: Option<&NatsToolProvider>,
) -> Vec<Arc<crate::config::script_tools::ResolvedScriptTool>> {
    let Some(provider) = provider else {
        return view;
    };
    view.into_iter()
        .filter(|tool| {
            let name = tool.definition().name.as_str();
            if provider.has_tool(name) {
                crate::config::script_tools::warn_collision(tool);
                return false;
            }
            true
        })
        .collect()
}

fn build_context_decl_map(
    mut declarations: Vec<ToolDeclaration>,
    provider_declarations: &[ToolDeclaration],
    command_bash: Option<&crate::bash_provider::CommandBashInputs>,
) -> HashMap<String, ToolDeclaration> {
    // Provider declarations follow the config declarations, so a base tool
    // named like a script tool wins the map — consistent with the accepted
    // view, which excludes any script tool the provider serves.
    declarations.extend(provider_declarations.iter().cloned());
    if let Some(inputs) = command_bash {
        // The synthetic outer `bash` wins against raw aliases, matching dispatch order.
        declarations.push(inputs.bash_declaration());
    }
    build_decl_map(declarations)
}

fn build_decl_map(tool_declarations: Vec<ToolDeclaration>) -> HashMap<String, ToolDeclaration> {
    tool_declarations
        .into_iter()
        .map(|declaration| (declaration.name.clone(), declaration))
        .collect()
}

fn build_confirm_tool_use_fn(config: &Config) -> Arc<ConfirmToolUseFn> {
    // Worker-side confirmation callback override (falls back to inquire prompt).
    config
        .tui_confirm_tool_use
        .clone()
        .unwrap_or_else(|| Arc::new(default_confirm_tool_use))
}

/// Assemble normal providers, with an optional command-scoped Bash catalog.
/// An override keeps Bash active even for pure shell and limits the inner
/// catalog to that command. Base providers never contain Bash, preventing recursion.
///
/// Script tools wrap the base providers and sit between `bash` and them
/// (decision 1), giving the call graph `bash -> script tool -> base` with no
/// reverse edges: the script provider's inner executor only ever holds base
/// providers, and the bash provider's inner set additionally contains the
/// script provider.
/// The base stack and the round's script-tool provider are assembled by the
/// helpers below; this function only decides the provider order.
/// Inputs for [`build_tool_providers`], one field per source of providers so
/// the assembler stays a pure function of its inputs. `nats_declarations`
/// and `script_tools` come from the round snapshot: the bash inner catalog
/// derives from the same captured sets as the round's advertisements.
struct BuildToolProvidersParams<'a> {
    config: &'a GlobalConfig,
    nats_provider: Option<Arc<crate::nats_tool_provider::NatsToolProvider>>,
    bash_context_slot: Option<Arc<crate::bash_provider::BashEvalContextSlot>>,
    command_bash: Option<crate::bash_provider::CommandBashInputs>,
    extra_base_providers: Vec<Arc<dyn ToolProvider>>,
    script_tools: Vec<Arc<crate::config::script_tools::ResolvedScriptTool>>,
    nats_declarations: Vec<ToolDeclaration>,
}

fn build_tool_providers(params: BuildToolProvidersParams<'_>) -> Vec<Arc<dyn ToolProvider>> {
    let BuildToolProvidersParams {
        config,
        nats_provider,
        bash_context_slot,
        command_bash,
        extra_base_providers,
        script_tools,
        nats_declarations,
    } = params;
    let operator_command = command_bash.is_some();
    let (bash_active, bash_inner_declarations) = match command_bash {
        Some(inputs) => (true, inputs.inner_declarations),
        None => {
            let guard = config.read();
            (
                guard.bash_tools_active(),
                guard.bash_inner_tool_declarations_with(&nats_declarations, &script_tools),
            )
        }
    };
    // N1: a context without a round slot cannot run script tool bodies, so
    // the script provider below holds none; the `bash` inner catalog must
    // not offer them as commands either — their dispatch could only fail.
    let bash_inner_declarations =
        bash_catalog_for_slot(config, &bash_context_slot, bash_inner_declarations);
    let stack = base_provider_stack(config, nats_provider, extra_base_providers);
    // Script tools wrap only base providers, so they can never dispatch to
    // `bash` or to each other (decision 1, R10). The round snapshot's script
    // tools are threaded in (decision 12): the same accepted view that
    // served this round's declarations, read once before the NATS discovery
    // await, is the one the provider serves.
    let script_provider =
        script_provider_for_round(script_tools, config, &stack, bash_context_slot.clone());

    // The script-mode `bash` provider must come before the NATS provider so
    // it wins `has_tool("bash")` against raw-name aliases registered from
    // tool servers (script mode, #2537); see [`script_mode_bash_provider`].
    let mut providers: Vec<Arc<dyn ToolProvider>> = Vec::new();
    providers.push(script_mode_bash_provider(ScriptModeBashParams {
        bash_active,
        bash_inner_declarations,
        stack: &stack,
        script_provider: &script_provider,
        slot: bash_context_slot,
        full_payload: operator_command,
    }));
    providers.push(script_provider);
    providers.extend(stack.base_providers.iter().cloned());
    providers
}

struct ScriptModeBashParams<'a> {
    bash_active: bool,
    bash_inner_declarations: Vec<ToolDeclaration>,
    stack: &'a BaseProviderStack,
    script_provider: &'a Arc<dyn ToolProvider>,
    slot: Option<Arc<crate::bash_provider::BashEvalContextSlot>>,
    full_payload: bool,
}

/// The script-mode `bash` provider. Its inner-call executor dispatches
/// through the script provider plus the base provider list, which
/// structurally excludes itself, so recursive `bash` dispatch is impossible
/// and `bash_tools` can admit script tools.
fn script_mode_bash_provider(params: ScriptModeBashParams<'_>) -> Arc<dyn ToolProvider> {
    let ScriptModeBashParams {
        bash_active,
        bash_inner_declarations,
        stack,
        script_provider,
        slot,
        full_payload,
    } = params;
    let base_providers: Vec<Arc<dyn ToolProvider>> = std::iter::once(script_provider.clone())
        .chain(stack.base_providers.iter().cloned())
        .collect();
    Arc::new(
        crate::bash_provider::BashProvider::new(crate::bash_provider::BashProviderConfig {
            active: bash_active,
            inner_declarations: bash_inner_declarations,
            base_providers,
            slot,
            payload_limit: stack.payload_limit.clone(),
            full_payload,
            script_outputs: stack.script_outputs.clone(),
        })
        .with_attachment_fetcher(stack.attachment_fetcher.clone())
        .with_media_store(stack.media_store.clone()),
    )
}

/// The base provider stack shared by the script-tool and `bash` providers:
/// every provider except `bash` and the round's script-tool provider, plus
/// the NATS-derived inputs both of them need.
struct BaseProviderStack {
    base_providers: Vec<Arc<dyn ToolProvider>>,
    payload_limit: Option<crate::bash_inner::PayloadLimit>,
    /// Output mappings by tool name, for the `bash` tool's commands.
    script_outputs: std::collections::BTreeMap<String, harnx_toolset::ScriptOutput>,
    attachment_fetcher: Option<Arc<dyn harnx_script::fs_attachments::AttachmentFetcher>>,
    media_store: Option<Arc<dyn crate::bash_inner::media::MediaStore>>,
}

/// NATS is the runtime provider for configured sub-agent and tool-server
/// tools. Its broker `max_payload` also feeds the script-mode pre-flight
/// check, and its stores feed the BashKit mounts of both script runners.
fn base_provider_stack(
    config: &GlobalConfig,
    nats_provider: Option<Arc<crate::nats_tool_provider::NatsToolProvider>>,
    extra_base_providers: Vec<Arc<dyn ToolProvider>>,
) -> BaseProviderStack {
    let payload_limit = nats_provider
        .as_ref()
        .map(|nats| nats.script_payload_limit());
    let script_outputs = nats_provider
        .as_ref()
        .map(|nats| nats.script_outputs())
        .unwrap_or_default();
    let attachment_fetcher = nats_provider
        .as_ref()
        .map(|nats| nats.script_attachment_fetcher());
    let media_store = nats_provider.as_ref().map(|nats| nats.script_media_store());
    let mut base_providers: Vec<Arc<dyn ToolProvider>> = nats_provider
        .map(|nats| nats as Arc<dyn ToolProvider>)
        .into_iter()
        .collect();
    base_providers.push(
        Arc::new(crate::session_history::SessionHistoryProvider::new(
            config.clone(),
        )) as Arc<dyn ToolProvider>,
    );
    base_providers.push(Arc::new(crate::session_meta_tool::SessionMetaProvider::new(
        config.clone(),
    )) as Arc<dyn ToolProvider>);
    base_providers.extend(extra_base_providers);
    BaseProviderStack {
        base_providers,
        payload_limit,
        script_outputs,
        attachment_fetcher,
        media_store,
    }
}

/// Drop declarations whose names belong to script tools, for contexts whose
/// script provider holds no tools (N1).
fn filter_script_tools(
    config: &GlobalConfig,
    declarations: Vec<ToolDeclaration>,
) -> Vec<ToolDeclaration> {
    let guard = config.read();
    let catalog = guard.script_tool_catalog();
    declarations
        .into_iter()
        .filter(|declaration| catalog.get(&declaration.name).is_none())
        .collect()
}

/// The `bash` inner catalog for this round: contexts without a round slot
/// cannot run script tool bodies, so the catalog drops script-tool
/// declarations there (N1); a slotted context keeps them so `bash` can call
/// the round's script tools.
fn bash_catalog_for_slot(
    config: &GlobalConfig,
    bash_context_slot: &Option<Arc<crate::bash_provider::BashEvalContextSlot>>,
    declarations: Vec<ToolDeclaration>,
) -> Vec<ToolDeclaration> {
    match bash_context_slot {
        Some(_) => declarations,
        None => filter_script_tools(config, declarations),
    }
}

/// The round's script-tool provider over the snapshot's accepted view,
/// wrapping the base providers (decision 12's shared snapshot). `config`
/// stays reachable for call-time inner-selector resolution (decision 5).
fn script_provider_for_round(
    script_tools: Vec<Arc<crate::config::script_tools::ResolvedScriptTool>>,
    config: &GlobalConfig,
    stack: &BaseProviderStack,
    bash_context_slot: Option<Arc<crate::bash_provider::BashEvalContextSlot>>,
) -> Arc<dyn ToolProvider> {
    Arc::new(crate::script_tool_provider::ScriptToolProvider::new(
        crate::script_tool_provider::ScriptToolProviderConfig {
            tools: script_tools,
            base_providers: stack.base_providers.clone(),
            config: config.clone(),
            slot: bash_context_slot,
            payload_limit: stack.payload_limit.clone(),
            attachment_fetcher: stack.attachment_fetcher.clone(),
            media_store: stack.media_store.clone(),
        },
    )) as Arc<dyn ToolProvider>
}

pub(crate) fn populate_result_markdown(
    results: Vec<ToolResult>,
    eval_ctx: &ToolEvalContext,
) -> Vec<ToolResult> {
    results
        .into_iter()
        .map(|mut result| {
            let raw_fallback = tool_result_raw_fallback(&result.output);
            result.markdown = eval_ctx.render.as_ref().and_then(|render| {
                render_result_for_display(
                    &result.call,
                    &result.output,
                    &raw_fallback,
                    render.decl_map.as_ref(),
                )
            });
            result
        })
        .collect()
}

fn tool_result_raw_fallback(output: &Value) -> String {
    extract_user_display_text(output).unwrap_or_else(|| match output {
        Value::String(text) => text.clone(),
        _ => pretty_yaml_block(output),
    })
}

/// Look up and render the call template for a tool, returning rendered string or None.
/// On template render error, logs a warning via `log::warn!` and falls back to
/// `raw_fallback` so display continues uninterrupted.
fn render_call(
    call: &ToolCall,
    json_data: &Value,
    raw_fallback: &str,
    decl_map: &HashMap<String, ToolDeclaration>,
) -> Option<String> {
    let tmpl = decl_map.get(&call.name)?.call_template.as_ref()?;
    Some(
        render_tool_call_template(tmpl, json_data, raw_fallback).unwrap_or_else(|e| {
            log::warn!("template error in tool '{}' call_template: {e}", call.name);
            raw_fallback.to_string()
        }),
    )
}

/// Look up and render result template for a tool, returning rendered string or None.
/// On template render error, logs a warning via `log::warn!` and falls back to
/// `raw_fallback` so display continues uninterrupted.
fn render_result(
    call: &ToolCall,
    result: &Value,
    raw_fallback: &str,
    decl_map: &HashMap<String, ToolDeclaration>,
) -> Option<String> {
    let tmpl = decl_map.get(&call.name)?.result_template.as_ref()?;
    Some(
        render_tool_result_template(tmpl, result, raw_fallback).unwrap_or_else(|e| {
            log::warn!(
                "template error in tool '{}' result_template: {e}",
                call.name
            );
            raw_fallback.to_string()
        }),
    )
}

/// Emit a tool call event with optional template rendering.
fn emit_tool_call_with_template(
    call: &ToolCall,
    json_data: &Value,
    decl_map: &HashMap<String, ToolDeclaration>,
) {
    use harnx_core::event::{AgentEvent, ToolEvent, ToolKind};

    let raw_fallback = match json_data {
        Value::Null => String::new(),
        _ => pretty_yaml_block(json_data),
    };

    let markdown = render_call(call, json_data, &raw_fallback, decl_map);

    let kind = decl_map
        .get(&call.name)
        .and_then(|decl| decl.kind)
        .unwrap_or(ToolKind::Other);

    let event = AgentEvent::Tool(ToolEvent::Started {
        id: call.id.clone().unwrap_or_default(),
        name: call.name.clone(),
        kind,
        markdown,
        input: json_data.clone(),
        locations: Vec::new(),
    });

    if !harnx_core::sink::emit_agent_event(event) && *IS_STDOUT_TERMINAL {
        print_tool_call_fallback(call, json_data, decl_map, &raw_fallback);
    }
}

/// Fallback print for tool call when no sink is installed.
fn print_tool_call_fallback(
    call: &ToolCall,
    json_data: &Value,
    decl_map: &HashMap<String, ToolDeclaration>,
    raw_fallback: &str,
) {
    if let Some(rendered) = render_call(call, json_data, raw_fallback, decl_map) {
        println!("[tool] {} {}", call.name, rendered);
    } else {
        let text = if raw_fallback.is_empty() {
            format!("[tool] {}", call.name)
        } else {
            format!("[tool] {} {raw_fallback}", call.name)
        };
        println!("{text}");
    }
}

/// Emit a tool result event with optional template rendering.
fn emit_tool_result_with_template(
    call: &ToolCall,
    result: &Value,
    decl_map: &HashMap<String, ToolDeclaration>,
) {
    use harnx_core::event::{AgentEvent, ToolEvent};

    let raw_fallback = extract_user_display_text(result).unwrap_or_else(|| match result {
        Value::String(s) => s.clone(),
        _ => pretty_yaml_block(result),
    });

    let markdown = render_result(call, result, &raw_fallback, decl_map);

    let event = AgentEvent::Tool(ToolEvent::Completed {
        id: call.id.clone().unwrap_or_default(),
        output: result.clone(),
        markdown: markdown.clone(),
    });

    if !harnx_core::sink::emit_agent_event(event) && *IS_STDOUT_TERMINAL {
        print_tool_result_fallback(call, result, decl_map, &raw_fallback);
    }
}

fn emit_tool_blocked_with_template(
    call: &ToolCall,
    blocked_result: &Value,
    decl_map: &HashMap<String, ToolDeclaration>,
) {
    use harnx_core::event::{AgentEvent, ToolEvent};

    let reason = blocked_result
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("blocked by hook")
        .to_string();

    let raw_fallback = match &call.arguments {
        Value::Null => String::new(),
        _ => pretty_yaml_block(&call.arguments),
    };
    let input_rendered = render_call(call, &call.arguments, &raw_fallback, decl_map);

    let event = AgentEvent::Tool(ToolEvent::Blocked {
        id: call.id.clone().unwrap_or_default(),
        name: call.name.clone(),
        input: call.arguments.clone(),
        reason,
    });
    let _ = input_rendered;
    let _ = harnx_core::sink::emit_agent_event(event);
}

/// Look up `call.name` in `decl_map`, render `call_template` against `json_data`
/// using `raw_fallback`. Returns `Some(rendered_markdown)` or `None`.
pub fn render_call_for_display(
    call: &ToolCall,
    json_data: &Value,
    raw_fallback: &str,
    decl_map: &HashMap<String, ToolDeclaration>,
) -> Option<String> {
    render_call(call, json_data, raw_fallback, decl_map)
}

/// Look up `call.name` in `decl_map`, render `result_template` against `result`
/// using `raw_fallback`. Returns `Some(rendered_markdown)` or `None`.
pub fn render_result_for_display(
    call: &ToolCall,
    result: &Value,
    raw_fallback: &str,
    decl_map: &HashMap<String, ToolDeclaration>,
) -> Option<String> {
    render_result(call, result, raw_fallback, decl_map)
}

/// Fallback print for tool result when no sink is installed. Routes
/// through the shared `render_tool_result_text` helper so this no-sink
/// path stays consistent with what the TUI/CLI sinks render.
fn print_tool_result_fallback(
    call: &ToolCall,
    result: &Value,
    decl_map: &HashMap<String, ToolDeclaration>,
    raw_fallback: &str,
) {
    let markdown = render_result(call, result, raw_fallback, decl_map);
    let truncated = render_tool_result_text(result, markdown.as_deref());
    println!("{}", dimmed_text(&truncated));
}

fn prompt_tool_confirmation(tool_name: &str, input: &Value, reason: Option<&str>) -> bool {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return false;
    }
    const MAX_CONFIRMATION_PREVIEW_CHARS: usize = 500;
    let preview: String = serde_json::to_string_pretty(input)
        .unwrap_or_default()
        .chars()
        .take(MAX_CONFIRMATION_PREVIEW_CHARS)
        .collect();
    let message = format!(
        "Confirm tool '{tool_name}'\n{}\nInput: {preview}\nAllow this tool call?",
        reason.unwrap_or_default()
    );
    inquire::Confirm::new(&message)
        .with_default(false)
        .prompt()
        .unwrap_or_default()
}

fn default_confirm_tool_use(
    call: &ToolCall,
    arguments: &Value,
    reason: Option<&str>,
) -> ToolUseConfirmation {
    if prompt_tool_confirmation(&call.name, arguments, reason) {
        ToolUseConfirmation::Approve
    } else {
        ToolUseConfirmation::Deny {
            reason: reason.map(str::to_string),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::config::ConfigLock;
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn handoff_preparation_stays_lazy_in_tool_eval_context() {
        let config = Arc::new(ConfigLock::new(Config::default()));
        let instance_id = harnx_core::instance::ServerScope::new();
        let prepare: Arc<PrepareHandoffFn> =
            Arc::new(|_, _, _| panic!("building a tool context must not prepare a handoff"));
        let mut params = BuildToolEvalContextParams::new(&config, &instance_id);
        params.prepare_handoff_fn = Some(prepare.clone());

        let context = build_tool_eval_context(params).await;

        assert!(Arc::ptr_eq(
            context
                .prepare_handoff_fn
                .as_ref()
                .expect("runtime callback"),
            &prepare,
        ));
    }

    /// The script-mode `bash` provider must sit first so it wins
    /// `has_tool("bash")` against NATS raw-name aliases, and must only claim
    /// the name while the active agent's `bash_tools` resolves to tools
    /// (script mode, #2537).
    #[test]
    fn bash_provider_leads_and_tracks_bash_tools() {
        let tools = harnx_core::tool::Tools::init_from_mcp(Some(vec![make_decl_with_templates(
            "fs_read", None, None,
        )]));
        let mut agent_config = harnx_core::agent_config::AgentConfig::from_prompt("test");
        agent_config.set_bash_tools(Some(vec!["fs_*".to_string()]));
        let config = Config {
            tools,
            agent: Some(crate::config::agent::Agent::new(agent_config)),
            ..Config::default()
        };
        let config = Arc::new(ConfigLock::new(config));

        let providers = build_tool_providers(BuildToolProvidersParams {
            config: &config,
            nats_provider: None,
            bash_context_slot: None,
            command_bash: None,
            extra_base_providers: Vec::new(),
            script_tools: Vec::new(),
            nats_declarations: Vec::new(),
        });
        assert_eq!(providers[0].name(), "bash-script");
        assert!(providers[0].has_tool("bash"));

        let bare = Arc::new(ConfigLock::new(Config::default()));
        let providers = build_tool_providers(BuildToolProvidersParams {
            config: &bare,
            nats_provider: None,
            bash_context_slot: None,
            command_bash: None,
            extra_base_providers: Vec::new(),
            script_tools: Vec::new(),
            nats_declarations: Vec::new(),
        });
        assert_eq!(providers[0].name(), "bash-script");
        assert!(
            !providers[0].has_tool("bash"),
            "inert when bash_tools resolves to nothing"
        );
    }

    /// #2630: command-scoped provider inputs keep the leading `bash`
    /// provider active even with an empty admitted catalog, while ordinary
    /// construction stays inert on the same config.
    #[test]
    fn command_bash_provider_stays_active_with_empty_catalog() {
        let config = Arc::new(ConfigLock::new(Config::default()));
        let providers = build_tool_providers(BuildToolProvidersParams {
            config: &config,
            nats_provider: None,
            bash_context_slot: None,
            command_bash: Some(crate::bash_provider::CommandBashInputs {
                inner_declarations: Vec::new(),
            }),
            extra_base_providers: Vec::new(),
            script_tools: Vec::new(),
            nats_declarations: Vec::new(),
        });
        assert_eq!(providers[0].name(), "bash-script");
        assert!(
            providers[0].has_tool("bash"),
            "command-scoped bash is active for pure shell"
        );
        for provider in &providers[1..] {
            assert!(
                !provider.has_tool("bash"),
                "{} must not claim the reserved name",
                provider.name()
            );
        }
    }

    /// #2630: a command-scoped context carries the synthetic `bash`
    /// declaration in its map and allowed names, so the operator paths can
    /// admit the command's `bash` call after their own overwrites.
    #[tokio::test]
    async fn command_bash_context_carries_synthetic_bash_declaration() {
        let config = Arc::new(ConfigLock::new(Config::default()));
        let instance_id = harnx_core::instance::ServerScope::new();
        let context = build_tool_eval_context(
            BuildToolEvalContextParams::new(&config, &instance_id).with_command_bash(
                crate::bash_provider::CommandBashInputs {
                    inner_declarations: vec![make_decl_with_templates("fs_read", None, None)],
                },
            ),
        )
        .await;

        assert!(context.allowed_tool_names.contains("bash"));
        let decl_map = context
            .render
            .as_ref()
            .expect("render context")
            .decl_map
            .as_ref();
        assert!(decl_map.contains_key("bash"), "synthetic bash declaration");
        assert!(context.providers[0].has_tool("bash"));
    }

    #[tokio::test]
    async fn a_stop_whose_results_cannot_be_persisted_still_reports_the_stop() {
        use harnx_core::loop_guard::{repetition_stop_sentence, RepetitionTerminal};
        let mut config = Config::default();
        // No pending tool-call message, so persisting the stopped round's
        // results fails.
        let session = crate::config::session::new(&config, "unpersisted_stop", None).unwrap();
        config.session = Some(session);
        let config = Arc::new(ConfigLock::new(config));
        let eval = build_tool_eval_context(BuildToolEvalContextParams::new(
            &config,
            &harnx_core::instance::ServerScope::new(),
        ))
        .await;
        let call = ToolCall::new(
            "fs_read".to_string(),
            json!({"path": "a.rs"}),
            Some("call_1".to_string()),
            None,
        );
        let guard = parking_lot::Mutex::new(harnx_core::loop_guard::ToolRepeatGuard::default());
        let now = chrono::Utc::now();
        for _ in 0..4 {
            guard.lock().record(&call, &json!("same"), now);
        }
        let calls = std::slice::from_ref(&call);
        let (_, refused) = screen_tool_round(Some(&guard), &config, calls, &eval)
            .await
            .expect("the first over-limit request is refused");
        assert_eq!(refused.len(), 1);

        let error = screen_tool_round(Some(&guard), &config, calls, &eval)
            .await
            .expect_err("asking again in the next round ends the turn");

        let terminal = RepetitionTerminal::tool_calls("fs_read", 4);
        assert_eq!(repetition_stop_sentence(&error), Some(terminal.sentence()));
        let persisted = format!("{error:#}");
        assert!(
            persisted.starts_with("failed to persist stopped tool results: "),
            "{persisted}"
        );
        assert_eq!(
            crate::parse_worker_terminal(&persisted),
            Some(crate::WorkerTerminal::Repetition(terminal))
        );
    }

    #[tokio::test]
    async fn instance_id_is_preserved_in_tool_eval_context() {
        let config = Arc::new(ConfigLock::new(Config::default()));
        let instance_id = harnx_core::instance::ServerScope::new();

        let context =
            build_tool_eval_context(BuildToolEvalContextParams::new(&config, &instance_id)).await;

        assert_eq!(context.instance_id, instance_id);
    }

    #[tokio::test]
    async fn a_tool_round_without_a_broker_address_starts_no_broker() {
        use crate::test_environment::{env_lock_async, EnvGuard};
        let _lock = env_lock_async().await;
        let data_dir = tempfile::tempdir().expect("create data dir");
        let _data_dir = EnvGuard::new("HARNX_DATA_DIR", data_dir.path());
        let _url = EnvGuard::remove(crate::config::HARNX_NATS_URL_ENV);
        let _token = EnvGuard::remove(crate::config::HARNX_NATS_TOKEN_ENV);
        let config = Arc::new(ConfigLock::new(Config::default()));

        let context = build_tool_eval_context(BuildToolEvalContextParams::new(
            &config,
            &harnx_core::instance::ServerScope::new(),
        ))
        .await;

        assert!(
            !harnx_core::config_paths::nats_runtime_dir().exists(),
            "tool discovery started or joined the shared local broker"
        );
        let providers: Vec<_> = context.providers.iter().map(|p| p.name()).collect();
        assert!(!providers.contains(&"nats"), "providers: {providers:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn build_tool_eval_context_stores_current_agent_package() {
        // The package derived from a qualified agent name in `execute_tool_round`
        // must be threaded into `ToolEvalContext` so the engine can resolve
        // same-package handoff targets (#709).
        let _guard = crate::client::TestStateGuard::new(None).await;
        let config = Arc::new(ConfigLock::new(Config::default()));

        // Mirror the derivation done in `execute_tool_round`.
        let pkg = harnx_core::package_namespace::pkg_from_qualified("pantheon/daedalus")
            .map(str::to_string);
        assert_eq!(pkg.as_deref(), Some("pantheon"));
        let ctx = build_tool_eval_context(
            BuildToolEvalContextParams::new(&config, &harnx_core::instance::ServerScope::new())
                .with_current_agent_package(pkg),
        )
        .await;
        assert_eq!(ctx.current_agent_package.as_deref(), Some("pantheon"));

        // A bare (top-level) agent name yields no package context.
        let bare =
            harnx_core::package_namespace::pkg_from_qualified("daedalus").map(str::to_string);
        assert_eq!(bare, None);
        let ctx = build_tool_eval_context(
            BuildToolEvalContextParams::new(&config, &harnx_core::instance::ServerScope::new())
                .with_current_agent_package(bare),
        )
        .await;
        assert_eq!(ctx.current_agent_package, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn build_tool_eval_context_uses_tui_confirm_override() {
        // When a worker-side confirmation callback is set, the eval context must
        // use it instead of the default inquire prompt (#695).
        let _guard = crate::client::TestStateGuard::new(None).await;
        let config = Arc::new(ConfigLock::new(Config::default()));

        // Default (no override): the inquire-based prompt is used. In a
        // non-terminal test process it denies, so this returns Deny.
        let ctx = build_tool_eval_context(BuildToolEvalContextParams::new(
            &config,
            &harnx_core::instance::ServerScope::new(),
        ))
        .await;
        let call = ToolCall::new("t".to_string(), serde_json::json!({}), None, None);
        assert!(matches!(
            (ctx.confirm_tool_use_fn)(&call, &serde_json::json!({}), None),
            ToolUseConfirmation::Deny { .. }
        ));

        // With an override installed, the context routes confirmation through it.
        config
            .write()
            .set_tui_confirm_tool_use(Some(Arc::new(|_, _, _| ToolUseConfirmation::Approve)));
        let ctx = build_tool_eval_context(BuildToolEvalContextParams::new(
            &config,
            &harnx_core::instance::ServerScope::new(),
        ))
        .await;
        assert!(matches!(
            (ctx.confirm_tool_use_fn)(&call, &serde_json::json!({}), None),
            ToolUseConfirmation::Approve
        ));
    }

    // ----------------------------------------------------------------
    // MCP MiniJinja templating, producer side: verify that
    // `emit_tool_call_with_template` and `emit_tool_result_with_template`
    // populate `markdown` on both `ToolEvent::Started` and
    // `ToolEvent::Completed` when the matching tool declaration carries
    // a `call_template` / `result_template`. These are the values the
    // TUI/CLI consumers then render — without these events being
    // populated, the consumer-side rendering has nothing to display.
    // ----------------------------------------------------------------

    use harnx_core::event::{AgentEvent, AgentEventSink, ToolEvent};
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct RecordingSink {
        events: StdMutex<Vec<AgentEvent>>,
    }
    impl AgentEventSink for RecordingSink {
        fn emit(&self, event: AgentEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn make_decl_with_templates(
        name: &str,
        call_template: Option<&str>,
        result_template: Option<&str>,
    ) -> ToolDeclaration {
        ToolDeclaration {
            name: name.to_string(),
            description: String::new(),
            parameters: Default::default(),
            mcp_tool_name: Some(name.to_string()),
            mcp_server_name: None,
            call_template: call_template.map(String::from),
            result_template: result_template.map(String::from),
            idempotent_hint: None,
            read_only_hint: None,
            kind: None,
        }
    }

    /// Lock around the global sink so producer-side emit tests don't race
    /// with each other. The sink is process-global state. Ignore poisoning
    /// so a panic in one test doesn't cascade-fail every other test that
    /// touches the sink.
    fn with_recording_sink<R>(test_body: impl FnOnce(Arc<RecordingSink>) -> R) -> R {
        static TEST_LOCK: StdMutex<()> = StdMutex::new(());
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        harnx_core::sink::clear_agent_event_sink();
        let sink: Arc<RecordingSink> = Arc::new(RecordingSink::default());
        harnx_core::sink::install_agent_event_sink(sink.clone());
        let result = test_body(sink);
        harnx_core::sink::clear_agent_event_sink();
        result
    }

    #[test]
    fn render_call_for_display_returns_rendered_when_template_exists() {
        let decl = make_decl_with_templates("bash_exec", Some("$ {{ args.command }}"), None);
        let mut decl_map = HashMap::new();
        decl_map.insert(decl.name.clone(), decl);
        let call = ToolCall::new(
            "bash_exec".to_string(),
            json!({"command": "ls -la /tmp"}),
            Some("call-1".to_string()),
            None,
        );
        let json_data = json!({"command": "ls -la /tmp"});
        let raw_fallback = "command: ls -la /tmp";
        let result = render_call_for_display(&call, &json_data, raw_fallback, &decl_map);
        assert_eq!(result, Some("$ ls -la /tmp".to_string()));
    }

    #[test]
    fn emit_tool_call_with_template_sets_started_markdown() {
        let decl = make_decl_with_templates("bash_exec", Some("$ {{ args.command }}"), None);
        let mut decl_map = HashMap::new();
        decl_map.insert(decl.name.clone(), decl);

        let call = ToolCall::new(
            "bash_exec".to_string(),
            json!({"command": "ls -la"}),
            Some("call-1".to_string()),
            None,
        );
        let args = json!({"command": "ls -la"});

        with_recording_sink(|sink| {
            emit_tool_call_with_template(&call, &args, &decl_map);
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 1, "expected one Started event");
            match &events[0] {
                AgentEvent::Tool(ToolEvent::Started { markdown, name, .. }) => {
                    assert_eq!(name, "bash_exec");
                    assert_eq!(
                        markdown.as_deref(),
                        Some("$ ls -la"),
                        "template should be rendered into Started.markdown"
                    );
                }
                other => panic!("expected Started event, got {other:?}"),
            }
        });
    }

    #[test]
    fn emit_tool_call_without_template_leaves_markdown_none() {
        let decl = make_decl_with_templates("plain_tool", None, None);
        let mut decl_map = HashMap::new();
        decl_map.insert(decl.name.clone(), decl);
        let call = ToolCall::new(
            "plain_tool".to_string(),
            json!({"x": 1}),
            Some("call-1".to_string()),
            None,
        );
        let args = json!({"x": 1});

        with_recording_sink(|sink| {
            emit_tool_call_with_template(&call, &args, &decl_map);
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 1);
            match &events[0] {
                AgentEvent::Tool(ToolEvent::Started { markdown, .. }) => {
                    assert!(markdown.is_none(), "no template => markdown must be None");
                }
                other => panic!("expected Started event, got {other:?}"),
            }
        });
    }

    #[test]
    fn emit_tool_call_with_kind_emits_declared_kind() {
        use harnx_core::event::ToolKind;

        // Declaration with kind
        let decl = ToolDeclaration {
            name: "fs_read".to_string(),
            description: String::new(),
            parameters: Default::default(),
            mcp_tool_name: None,
            mcp_server_name: None,
            call_template: None,
            result_template: None,
            idempotent_hint: None,
            read_only_hint: None,
            kind: Some(ToolKind::Read),
        };
        let mut decl_map = HashMap::new();
        decl_map.insert(decl.name.clone(), decl);
        let call = ToolCall::new(
            "fs_read".to_string(),
            json!({"path": "/tmp/file.txt"}),
            Some("call-2".to_string()),
            None,
        );

        with_recording_sink(|sink| {
            emit_tool_call_with_template(&call, &json!({"path": "/tmp/file.txt"}), &decl_map);
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 1);
            match &events[0] {
                AgentEvent::Tool(ToolEvent::Started { kind, name, .. }) => {
                    assert_eq!(name, "fs_read");
                    assert_eq!(*kind, ToolKind::Read, "declared kind must be emitted");
                }
                other => panic!("expected Started event, got {other:?}"),
            }
        });
    }

    #[test]
    fn emit_tool_call_without_kind_emits_other() {
        use harnx_core::event::ToolKind;

        // Declaration without kind
        let decl = ToolDeclaration {
            name: "unknown_tool".to_string(),
            description: String::new(),
            parameters: Default::default(),
            mcp_tool_name: None,
            mcp_server_name: None,
            call_template: None,
            result_template: None,
            idempotent_hint: None,
            read_only_hint: None,
            kind: None,
        };
        let mut decl_map = HashMap::new();
        decl_map.insert(decl.name.clone(), decl);
        let call = ToolCall::new(
            "unknown_tool".to_string(),
            json!({}),
            Some("call-3".to_string()),
            None,
        );

        with_recording_sink(|sink| {
            emit_tool_call_with_template(&call, &json!({}), &decl_map);
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 1);
            match &events[0] {
                AgentEvent::Tool(ToolEvent::Started { kind, .. }) => {
                    assert_eq!(*kind, ToolKind::Other, "missing kind must default to Other");
                }
                other => panic!("expected Started event, got {other:?}"),
            }
        });
    }

    #[test]
    fn emit_tool_call_unknown_tool_emits_other() {
        use harnx_core::event::ToolKind;

        // Empty declaration map
        let decl_map: HashMap<String, ToolDeclaration> = HashMap::new();
        let call = ToolCall::new(
            "nonexistent_tool".to_string(),
            json!({}),
            Some("call-4".to_string()),
            None,
        );

        with_recording_sink(|sink| {
            emit_tool_call_with_template(&call, &json!({}), &decl_map);
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 1);
            match &events[0] {
                AgentEvent::Tool(ToolEvent::Started { kind, .. }) => {
                    assert_eq!(*kind, ToolKind::Other, "unknown tool must emit Other");
                }
                other => panic!("expected Started event, got {other:?}"),
            }
        });
    }

    #[test]
    fn emit_tool_result_with_template_sets_completed_markdown() {
        let decl =
            make_decl_with_templates("bash_exec", None, Some("OK: {{ result.content[0].text }}"));
        let mut decl_map = HashMap::new();
        decl_map.insert(decl.name.clone(), decl);

        let call = ToolCall::new(
            "bash_exec".to_string(),
            json!({}),
            Some("call-1".to_string()),
            None,
        );
        let result_json = json!({
            "content": [{"type": "text", "text": "hello"}],
            "isError": false,
        });

        with_recording_sink(|sink| {
            emit_tool_result_with_template(&call, &result_json, &decl_map);
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 1, "expected one Completed event");
            match &events[0] {
                AgentEvent::Tool(ToolEvent::Completed { markdown, .. }) => {
                    assert_eq!(
                        markdown.as_deref(),
                        Some("OK: hello"),
                        "template should be rendered into Completed.markdown"
                    );
                }
                other => panic!("expected Completed event, got {other:?}"),
            }
        });
    }

    /// Issue #1537: a recoverable-error tool result is shaped
    /// `{"is_error": true, "error": ...}` (no `content`). The shared plans
    /// result template `{{ result.content[0].text | default('') }}` must render
    /// it without a MiniJinja "undefined value" error — the `default('')` has to
    /// win, yielding empty markdown, instead of the template blowing up and
    /// logging `warn!("template error ...")`.
    #[test]
    fn render_result_for_display_handles_error_shape_without_content() {
        let decl = make_decl_with_templates(
            "get_note",
            None,
            Some("{{ result.content[0].text | default('') }}"),
        );
        let mut decl_map = HashMap::new();
        decl_map.insert(decl.name.clone(), decl);
        let call = ToolCall::new(
            "get_note".to_string(),
            json!({"plan": "p", "note_id": "note-1"}),
            Some("call-1".to_string()),
            None,
        );
        let error_result = json!({
            "is_error": true,
            "error": "note note-1 not found in plan 'p'",
        });
        let rendered = render_result_for_display(&call, &error_result, "raw fallback", &decl_map);
        // Some("") — the template rendered successfully to empty, NOT None
        // (which would mean no template) and NOT the raw fallback (which would
        // mean the template raised and got caught).
        assert_eq!(
            rendered.as_deref(),
            Some(""),
            "error-shape result must render the template to empty, not raise"
        );
    }

    #[test]
    fn render_result_for_display_returns_none_when_no_template() {
        let decl = make_decl_with_templates("bash_exec", None, None);
        let mut decl_map = HashMap::new();
        decl_map.insert(decl.name.clone(), decl);
        let call = ToolCall::new(
            "bash_exec".to_string(),
            json!({"command": "ls"}),
            Some("call-1".to_string()),
            None,
        );
        let result_val = json!({"output": "file.txt"});
        let result = render_result_for_display(&call, &result_val, "raw fallback", &decl_map);
        assert!(result.is_none(), "no result_template => should return None");
    }

    #[test]
    fn populate_result_markdown_renders_templates_and_leaves_missing_templates_none() {
        let mut decl_map = HashMap::new();
        decl_map.insert(
            "bash_exec".to_string(),
            make_decl_with_templates("bash_exec", None, Some("OK: {{ result.content[0].text }}")),
        );
        let eval_ctx = ToolEvalContext {
            structured_results: false,
            on_tool_call_completed: None,
            on_interaction: None,
            middleware_fn: None,
            work_boundary: None,
            instance_id: harnx_core::instance::ServerScope::new(),
            tool_round: None,
            render: Some(ToolEvalRenderContext {
                decl_map: Arc::new(decl_map),
            }),
            providers: Vec::new(),
            allowed_tool_names: HashSet::new(),
            current_agent_package: None,
            handoff_targets: HashMap::new(),
            emit_tool_call_fn: Arc::new(|_, _| {}),
            emit_tool_result_fn: Arc::new(|_, _| {}),
            emit_tool_blocked_fn: Arc::new(|_, _| {}),
            emit_tool_update_fn: Arc::new(|_, _| {}),
            confirm_tool_use_fn: Arc::new(|_, _, _| ToolUseConfirmation::Approve),
            prepare_handoff_fn: None,
        };

        let results = populate_result_markdown(
            vec![
                ToolResult::new(
                    ToolCall::new(
                        "bash_exec".to_string(),
                        json!({"command": "echo hi"}),
                        Some("call-1".to_string()),
                        None,
                    ),
                    json!({"content": [{"type": "text", "text": "hello"}]}),
                ),
                ToolResult::new(
                    ToolCall::new(
                        "plain_tool".to_string(),
                        json!({"command": "echo hi"}),
                        Some("call-2".to_string()),
                        None,
                    ),
                    json!({"content": [{"type": "text", "text": "raw"}]}),
                ),
            ],
            &eval_ctx,
        );

        assert_eq!(results[0].markdown.as_deref(), Some("OK: hello"));
        assert!(results[1].markdown.is_none());
    }

    #[tokio::test]
    async fn tool_update_emitter_captures_scoped_sink_across_spawn() {
        let sink = Arc::new(RecordingSink::default());
        let captured = harnx_core::sink::with_agent_event_sink(sink.clone(), async {
            let declarations = Arc::new(HashMap::new());
            build_emit_fns(&declarations).3
        })
        .await;

        tokio::spawn(async move {
            captured(
                "call-captured",
                &ToolUpdatePatch {
                    title: Some("spawned update".to_string()),
                    ..Default::default()
                },
            );
        })
        .await
        .expect("spawned update task");

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::Tool(ToolEvent::Update { id, title, .. }) => {
                assert_eq!(id, "call-captured");
                assert_eq!(title.as_deref(), Some("spawned update"));
            }
            other => panic!("expected tool update, got {other:?}"),
        }
    }

    #[test]
    fn emit_tool_result_without_template_leaves_markdown_none() {
        let decl = make_decl_with_templates("plain_tool", None, None);
        let mut decl_map = HashMap::new();
        decl_map.insert(decl.name.clone(), decl);
        let call = ToolCall::new(
            "plain_tool".to_string(),
            json!({}),
            Some("call-1".to_string()),
            None,
        );
        let result_json = json!({"content": [{"type": "text", "text": "hi"}]});

        with_recording_sink(|sink| {
            emit_tool_result_with_template(&call, &result_json, &decl_map);
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 1);
            match &events[0] {
                AgentEvent::Tool(ToolEvent::Completed { markdown, .. }) => {
                    assert!(
                        markdown.is_none(),
                        "no template => markdown must be None (consumer falls back to output)"
                    );
                }
                other => panic!("expected Completed event, got {other:?}"),
            }
        });
    }

    fn script_tool_definition(name: &str) -> harnx_core::script_tool::ScriptToolDefinition {
        serde_json::from_value(json!({
            "name": name,
            "description": "Wrapper.",
            "parameters": {
                "type": "object",
                "properties": {"number": {"type": "integer"}},
                "required": ["number"],
                "additionalProperties": false
            },
            "bash": "true"
        }))
        .unwrap()
    }

    fn config_with_script_tool(name: &str) -> Arc<ConfigLock> {
        config_with_script_tools(&[name])
    }

    fn config_with_script_tools(names: &[&str]) -> Arc<ConfigLock> {
        let definitions: Vec<_> = names
            .iter()
            .map(|name| script_tool_definition(name))
            .collect();
        let mut config = Config::default();
        config
            .set_script_tool_definitions(&definitions)
            .expect("test script tool resolves");
        Arc::new(ConfigLock::new(config))
    }

    /// C6/decision 12: the round snapshots the accepted view once, and the
    /// provider serves exactly that snapshot. A NATS/MCP refresh landing
    /// after the snapshot (here: a base tool registering the script tool's
    /// name) cannot make the round advertise a script tool it will not
    /// serve — or serve one it did not advertise.
    #[test]
    fn round_snapshot_keeps_declarations_and_provider_on_one_view() {
        let config = config_with_script_tool("wrap_probe");
        let snapshot = snapshot_tool_round(&config.read(), None, None, None, true);
        assert!(
            snapshot
                .declarations
                .iter()
                .any(|declaration| declaration.name == "wrap_probe"),
            "the snapshot must advertise the script tool"
        );

        // The mid-round refresh: a base tool takes the name. The current
        // accepted view drops the script tool, but this round's snapshot and
        // its provider stay the pair they were.
        config
            .write()
            .nats_tool_declarations
            .write()
            .push(make_decl_with_templates("wrap_probe", None, None));
        assert!(
            config
                .read()
                .accepted_script_tools()
                .iter()
                .all(|tool| tool.definition().name != "wrap_probe"),
            "a fresh view must drop the colliding script tool"
        );

        let stack = base_provider_stack(&config, None, Vec::new());
        let provider =
            script_provider_for_round(snapshot.script_tools.clone(), &config, &stack, None);
        assert!(
            provider.has_tool("wrap_probe"),
            "the provider must serve the view the round advertised"
        );
    }

    /// N1: a context without a round slot (hooks, operator `.call`,
    /// reservations) must not advertise script tools it could never run.
    #[test]
    fn round_snapshot_without_slot_carries_no_script_tools() {
        let config = config_with_script_tool("wrap_probe");
        let snapshot = snapshot_tool_round(&config.read(), None, Some("*"), None, false);
        assert!(
            snapshot.script_tools.is_empty(),
            "no slot means no script tools"
        );
        assert!(
            !snapshot
                .declarations
                .iter()
                .any(|declaration| declaration.name == "wrap_probe"),
            "the wildcard catalog must not advertise the script tool"
        );
    }

    /// 4659980786: a script tool whose name the round's NATS provider serves
    /// is excluded from the accepted view, exactly like a config-declared
    /// base tool — the round cannot advertise a base schema and then
    /// dispatch the script body under the same name.
    #[test]
    fn accepted_view_excludes_provider_served_names() {
        let config = config_with_script_tool("wrap_probe");
        let guard = config.read();
        let provider_declarations = vec![make_decl_with_templates("wrap_probe", None, None)];
        let base_names = guard.base_tool_names_with(&[], &provider_declarations);
        let view = guard.accepted_script_tools_against(&base_names);
        assert!(
            view.iter()
                .all(|tool| tool.definition().name != "wrap_probe"),
            "a provider-served name must drop the colliding script tool"
        );
    }

    /// 4659980786 follow-up: `has_tool` keeps raw aliases routable through
    /// the naming transition, but they never appear in the provider's
    /// `use_tools`-filtered declarations. A script tool named after a raw
    /// alias must still leave the accepted view: the script provider
    /// precedes the NATS provider in dispatch order, so it would shadow
    /// the alias. The non-colliding sibling must survive the exclusion.
    #[tokio::test]
    async fn script_tool_view_excludes_provider_raw_aliases() -> anyhow::Result<()> {
        let server = crate::nats_test_common::spawn_nats_server()
            .await?
            .ok_or_else(|| anyhow::anyhow!("nats-server required"))?;
        let client = async_nats::connect(server.url()).await?;
        // `fs` serves `read` with the agent-visible name `fs_read`; the raw
        // alias `read` routes but is not a declaration name.
        let provider = NatsToolProvider::snapshot_test_provider(
            &client,
            vec![("read", "fs", "read")],
            vec![make_decl_with_templates("fs_read", None, None)],
            Vec::new(),
        )
        .await?;
        assert!(provider.has_tool("read"), "raw alias must be routable");
        assert!(
            provider
                .declarations_for_use_tools(Some("*"))
                .iter()
                .all(|declaration| declaration.name != "read"),
            "the raw alias never appears in the filtered declarations"
        );

        let config = config_with_script_tools(&["read", "probe_read"]);
        // The model-visible tool list comes from `select_tools`, whose
        // collision view is the config-level route-name cache — populate it
        // the way `refresh_nats_tool_declarations` would.
        *config.read().nats_served_tool_names.write() = provider.route_names().collect();
        let mut agent_config = harnx_core::agent_config::AgentConfig::from_prompt("test");
        agent_config.set_use_tools(Some(vec!["*".to_string()]));
        let functions = config.read().select_tools(&agent_config);
        assert!(
            functions
                .as_deref()
                .unwrap_or_default()
                .iter()
                .all(|declaration| declaration.name != "read"),
            "the model-visible list must not advertise the raw-alias collision"
        );
        assert!(
            functions
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|declaration| declaration.name == "probe_read"),
            "the model-visible list must keep the non-colliding sibling"
        );

        let snapshot = snapshot_tool_round(&config.read(), Some(&provider), Some("*"), None, true);
        assert!(
            snapshot
                .script_tools
                .iter()
                .all(|tool| tool.definition().name != "read"),
            "a raw-alias collision must drop the script tool"
        );
        assert!(
            snapshot
                .script_tools
                .iter()
                .any(|tool| tool.definition().name == "probe_read"),
            "the round view must keep the non-colliding sibling"
        );
        assert!(
            !snapshot
                .declarations
                .iter()
                .any(|declaration| declaration.name == "read"),
            "the round must not advertise the shadowed name"
        );
        assert!(
            snapshot
                .declarations
                .iter()
                .any(|declaration| declaration.name == "probe_read"),
            "the round must advertise the non-colliding sibling"
        );
        Ok(())
    }

    /// 4659980786 follow-up: a script tool selected only through
    /// `bash_tools` — the `use_tools` selectors don't name it, and the
    /// config's NATS declarations are stale — would reach the inner bash
    /// catalog under a name the provider serves, and the script provider
    /// would win dispatch over it. The provider-`has_tool` exclusion catches
    /// what the declaration-based check cannot. The non-colliding sibling
    /// (selected by both `use_tools` and `bash_tools`) must survive.
    #[tokio::test]
    async fn script_tool_view_excludes_provider_names_behind_bash_only_selector(
    ) -> anyhow::Result<()> {
        let server = crate::nats_test_common::spawn_nats_server()
            .await?
            .ok_or_else(|| anyhow::anyhow!("nats-server required"))?;
        let client = async_nats::connect(server.url()).await?;
        // The provider serves `wrap`; the config's NATS declarations are stale
        // and `use_tools` selects nothing, so no declaration-based exclusion
        // can see the collision.
        let provider = NatsToolProvider::snapshot_test_provider(
            &client,
            vec![("wrap", "fs", "wrap")],
            vec![make_decl_with_templates("wrap", None, None)],
            Vec::new(),
        )
        .await?;
        assert!(provider.has_tool("wrap"));

        let config = config_with_script_tools(&["wrap", "wrap_report"]);
        {
            let mut guard = config.write();
            let mut agent_config = harnx_core::agent_config::AgentConfig::from_prompt("test");
            // `wrap` is bash-only and provider-shadowed; the sibling is
            // selected by both `use_tools` and `bash_tools`.
            agent_config.set_use_tools(Some(vec!["wrap_report".to_string()]));
            agent_config.set_bash_tools(Some(vec!["wrap".to_string(), "wrap_report".to_string()]));
            guard.agent = Some(crate::config::agent::Agent::new(agent_config));
        }
        let guard = config.read();
        assert!(
            guard.bash_tools_active(),
            "the script tool must resolve as bash-only"
        );

        let snapshot =
            snapshot_tool_round(&guard, Some(&provider), Some("wrap_report"), None, true);
        assert!(
            snapshot
                .script_tools
                .iter()
                .all(|tool| tool.definition().name != "wrap"),
            "a provider-served name must drop the script tool even when it is only bash-selected"
        );
        assert!(
            snapshot
                .script_tools
                .iter()
                .any(|tool| tool.definition().name == "wrap_report"),
            "the round view must keep the non-colliding sibling"
        );
        // The inner bash catalog derives from the same snapshot, so the
        // shadowed name cannot come back as a shell command — and the
        // sibling stays available both as a command and as a declaration.
        let inner = guard
            .bash_inner_tool_declarations_with(&snapshot.nats_declarations, &snapshot.script_tools);
        assert!(
            inner.iter().all(|declaration| declaration.name != "wrap"),
            "the bash inner catalog must not offer the shadowed name"
        );
        assert!(
            inner
                .iter()
                .any(|declaration| declaration.name == "wrap_report"),
            "the bash inner catalog must keep the non-colliding sibling"
        );
        assert!(
            snapshot
                .declarations
                .iter()
                .any(|declaration| declaration.name == "wrap_report"),
            "the round must advertise the non-colliding sibling"
        );
        Ok(())
    }

    /// 4659980786 follow-up: `route_names` must cover everything `has_tool`
    /// accepts. A cross-package registration resolves under its
    /// per-registration agent-visible name through the bare-name fallback
    /// (`server_identity.rs`) without being a route-map key, so the shared
    /// collision view must yield that name as well as the map's raw aliases
    /// and visible names.
    #[tokio::test]
    async fn script_tool_view_route_names_cover_visible_names_and_raw_aliases() -> anyhow::Result<()>
    {
        let server = crate::nats_test_common::spawn_nats_server()
            .await?
            .ok_or_else(|| anyhow::anyhow!("nats-server required"))?;
        let client = async_nats::connect(server.url()).await?;
        // A registration from package `q`, server `fs`, tool `read`. The
        // route map keys the qualified name `q__fs_read` and the raw alias
        // `read`; the bare name `fs_read` routes only through the fallback.
        let registration: harnx_toolset::Registration = serde_json::from_value(json!({
            "package": "q",
            "config": "default",
            "server": "fs",
            "tools": [{
                "name": "read",
                "description": "fs read",
                "input_schema": {"type": "object"},
                "idempotent_hint": false,
                "read_only_hint": true
            }],
            "schema_version": 1,
            "proto_version": 1
        }))?;
        let provider = NatsToolProvider::snapshot_test_provider(
            &client,
            vec![("q__fs_read", "q__fs", "read"), ("read", "fs", "read")],
            Vec::new(),
            vec![registration],
        )
        .await?;
        assert!(provider.has_tool("read"), "the raw alias must route");
        assert!(
            provider.has_tool("fs_read"),
            "the bare name must route through the fallback"
        );
        let names: std::collections::HashSet<String> = provider.route_names().collect();
        assert!(
            names.contains("read") && names.contains("q__fs_read"),
            "route names must include the raw alias and the visible name"
        );
        assert!(
            names.contains("fs_read"),
            "route names must include the per-registration visible name"
        );
        Ok(())
    }
}

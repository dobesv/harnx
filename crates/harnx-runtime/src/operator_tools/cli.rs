//! Noninteractive operator entry points. Named agents stay on their worker;
//! no-agent commands own the merged virtual-session reservation.

use super::{
    evaluate, request_command_inner, subject, OperatorToolCommand, OperatorToolReply,
    OperatorToolRequest,
};
use crate::config::Config;
use crate::nats_tool_provider::{NatsInFlightCalls, NatsToolProvider};
use crate::nats_worker::tool_reservation::ToolReservationView;
use crate::tool_context::BuildToolEvalContextParams;
use crate::tool_output::ToolOutputFormat;
use crate::tool_reservation_client::{ToolReservationHandle, ToolReservationState};
use crate::utils::AbortSignal;
use anyhow::{ensure, Context, Result};
use std::{collections::HashSet, sync::Arc, time::Duration};

/// Captured caller selection for a named, noninteractive command.
pub struct NamedToolTarget<'a> {
    pub agent: &'a str,
    pub cluster: &'a str,
    pub route: crate::SessionActivationRoute,
}

/// Per-command output and cancellation policy, shared by both CLI caller modes.
#[derive(Clone, Copy)]
pub struct ToolCommandExecution<'a> {
    pub command: &'a OperatorToolCommand,
    pub json: bool,
    pub abort: &'a AbortSignal,
    /// Positive requested script timeout for a Bash command (#2630); `None`
    /// keeps the script backend default. The operator envelope in
    /// `tool_cmd` is unchanged.
    pub bash_timeout_secs: Option<u64>,
}

pub async fn run_named_tool_command(
    config: &Config,
    target: NamedToolTarget<'_>,
    execution: ToolCommandExecution<'_>,
) -> Result<OperatorToolReply> {
    let NamedToolTarget {
        agent,
        cluster,
        route,
    } = target;
    let ToolCommandExecution {
        command,
        json,
        abort,
        bash_timeout_secs,
    } = execution;
    let config = Arc::new(crate::config::ConfigLock::new(config.clone()));
    let session = crate::NatsSession::from_global_config(
        crate::NatsSessionConfig {
            cluster: cluster.into(),
            initializer: crate::SessionInitializer::named(agent, Default::default()),
            session_id: None,
            activation_route: route.clone(),
        },
        &config,
        abort.clone(),
    )
    .await?;
    // Subagent tools append progress to their caller even without a root prompt.
    let snapshot = config.read().clone();
    let replicas = snapshot
        .resolve_nats_server(cluster)
        .await?
        .resolved_replicas();
    crate::nats_session_log::NatsSessionLog::new_with_replicas(
        session.jetstream().clone(),
        session.storage_key(),
        replicas,
    )
    .last_entry_async()
    .await
    .context("failed to create named tool caller transcript")?;
    let request = OperatorToolRequest {
        version: 1,
        session_key: session.storage_key().into(),
        // The worker resolves the named agent's configuration, including packages.
        use_tools: None,
        tool_use: config.read().tool_use,
        command: command.clone(),
        json,
        bash_timeout_secs,
    };
    // CLI keeps this future alive through its cancellation drain, so it awaits
    // the existing acknowledgement/cancel handshake instead of detaching it.
    request_command_inner(
        &session.jetstream().client(),
        &subject(cluster, &route)?,
        &request,
        abort,
    )
    .await
}

pub fn ensure_current(
    reservation: &ToolReservationHandle,
    admitted: &ToolReservationState,
) -> Result<()> {
    ensure!(
        admitted.server_scope.is_some() && reservation.state() == *admitted,
        "Tool reservation changed or is unavailable; command was not admitted"
    );
    Ok(())
}

pub async fn run_reserved_tool_command(
    reservation: &ToolReservationHandle,
    view: &ToolReservationView,
    execution: ToolCommandExecution<'_>,
) -> Result<OperatorToolReply> {
    let ToolCommandExecution { command, abort, .. } = execution;
    let admitted = reservation.state();
    let scope = admitted
        .server_scope
        .clone()
        .context("Tool reservation is unavailable")?;
    let mut config = reservation.config().clone();
    let executes_tools = matches!(
        command,
        OperatorToolCommand::Call { .. } | OperatorToolCommand::Bash { .. }
    );
    if executes_tools && config.run_context.is_none() {
        config.run_context = Some(reservation.admit_external_call().await?);
    }
    // #2630: agentless Bash evaluates on a command-scoped context with the
    // slot-installed owner; other commands keep the plain by-value context.
    let context = if let OperatorToolCommand::Bash { selection, .. } = command {
        ReservedContext::Bash(reserved_bash_context(config, view, &scope, selection).await?)
    } else {
        ReservedContext::Plain(Box::new(reserved_eval_context(config, view, &scope).await?))
    };
    let mut state = reservation.subscribe();
    let observed = state.borrow_and_update().clone();
    ensure!(
        observed == admitted,
        "Tool reservation changed during discovery; command was not admitted"
    );
    ensure_current(reservation, &admitted)?;
    // Selection above already applies package sanitization and configured toolsets.
    let call = context.evaluate(execution);
    tokio::pin!(call);
    tokio::select! {
        biased;
        _ = state.changed() => {
            abort.set_ctrlc();
            // Never replay a side effect after scope loss. Give transport cancellation
            // time to settle, retaining any full result that arrived during cleanup.
            let result = tokio::time::timeout(Duration::from_secs(10), &mut call).await;
            let mut reply = result.ok().and_then(Result::ok).unwrap_or(OperatorToolReply {
                output: String::new(), error: None,
            });
            reply.error = Some("Tool reservation changed during execution; outcome may be unknown".into());
            Ok(reply)
        }
        result = &mut call => result,
    }
}

fn selected_provider_names(
    config: &Config,
    provider: &NatsToolProvider,
    view: &ToolReservationView,
) -> HashSet<String> {
    let requested: HashSet<_> = config
        .select_tools_for_package(&view.use_tools, view.package.as_deref())
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    // Match the MCP reservation view: export provider declarations, not generated
    // handoff/history tools or raw routing aliases from the local Config inventory.
    provider
        .declarations()
        .iter()
        .filter(|tool| requested.contains(&tool.name))
        .map(|tool| tool.name.clone())
        .collect()
}

/// Which kind of evaluation context one reserved command runs on (#2630).
enum ReservedContext {
    Plain(Box<harnx_engine::tool::ToolEvalContext>),
    Bash(ReservedBashContext),
}

impl ReservedContext {
    fn evaluate(
        self,
        execution: ToolCommandExecution<'_>,
    ) -> futures_util::future::LocalBoxFuture<'_, Result<OperatorToolReply>> {
        let ToolCommandExecution {
            command,
            json,
            abort,
            bash_timeout_secs,
        } = execution;
        let format = ToolOutputFormat::from_json_flag(json);
        match (self, command) {
            (ReservedContext::Bash(owner), OperatorToolCommand::Bash { script, .. }) => {
                Box::pin(evaluate_bash_owned(
                    owner,
                    super::BashInvocation {
                        script,
                        timeout_secs: bash_timeout_secs,
                        format,
                        abort,
                    },
                ))
            }
            (ReservedContext::Plain(ctx), command) => {
                let command = match command {
                    OperatorToolCommand::List { .. } => OperatorToolCommand::List { pattern: None },
                    _ => command.clone(),
                };
                Box::pin(async move { evaluate(*ctx, &command, format, abort).await })
            }
            // The context kind always matches the command kind.
            (ReservedContext::Bash(_), _) => unreachable!(),
        }
    }
}

/// Command-scoped context for one agentless Bash command (#2630): the
/// strong slot owner is held by the caller for the whole run.
struct ReservedBashContext {
    owner: Arc<harnx_engine::tool::ToolEvalContext>,
    root_call_id: String,
}

/// Agentless Bash admission (#2630): inferred commands admit the exact
/// literal-name intersection with the discovered inventory — never
/// selector/glob/toolset expansion — while explicit commands resolve their
/// own selectors through the normal package/toolset/glob path and admit only
/// the intersection with the reservation view. The view bounds what the
/// command may use at all, so warm inventory outside the reservation stays
/// unavailable, and empty or unmatched selectors admit nothing (pure shell
/// still runs). Control tools stay out of the script catalog in both modes.
/// Disabled tool use (`config.tool_use`) admits nothing in any mode, matching
/// the `select_tools_for_package` gate direct `call tool` passes through
/// (#2655).
fn reserved_bash_admitted_declarations(
    config: &Config,
    provider: &NatsToolProvider,
    view: &ToolReservationView,
    selection: &super::BashToolSelection,
) -> Vec<harnx_core::tool::ToolDeclaration> {
    if !config.tool_use {
        return Vec::new();
    }
    let discovered = provider.declarations();
    let resolved: HashSet<String> = match selection {
        // Inferred literals are bounded by the view's admitted set, as are
        // explicit commands. A CLI-derived view can resolve an inferred name
        // as a configured toolset and omit the same-named literal tool.
        super::BashToolSelection::Inferred { names } => {
            let view_names = selected_provider_names(config, provider, view);
            names
                .iter()
                .cloned()
                .collect::<HashSet<_>>()
                .intersection(&view_names)
                .cloned()
                .collect()
        }
        super::BashToolSelection::Explicit { selectors } => {
            explicit_admitted_within_view(config, provider, view, selectors)
        }
        // Session mode is TUI-only and never reaches the CLI reservation.
        super::BashToolSelection::Session => HashSet::new(),
    };
    super::inner_declarations_for(&resolved, discovered)
}

/// Explicit agentless admission: resolve the command's own selectors with the
/// same package/toolset/glob semantics the reservation view itself used, then
/// intersect the resolved names with the view's admitted set.
fn explicit_admitted_within_view(
    config: &Config,
    provider: &NatsToolProvider,
    view: &ToolReservationView,
    selectors: &[String],
) -> HashSet<String> {
    let view_names = selected_provider_names(config, provider, view);
    let requested: HashSet<String> = config
        .select_tools_for_package(selectors, view.package.as_deref())
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    view_names.intersection(&requested).cloned().collect()
}

async fn reserved_bash_context(
    mut config: Config,
    view: &ToolReservationView,
    scope: &harnx_core::instance::ServerScope,
    selection: &super::BashToolSelection,
) -> Result<ReservedBashContext> {
    let provider = tokio::time::timeout(
        Duration::from_secs(60),
        NatsToolProvider::discover_strict(
            &config,
            scope.clone(),
            NatsInFlightCalls::default(),
            view.package.as_deref(),
        ),
    )
    .await
    .context("Tool discovery timed out")??;
    // Config clones share the cache. A reservation's catalog must remain private.
    config.nats_tool_declarations =
        Arc::new(parking_lot::RwLock::new(provider.declarations().to_vec()));
    let admitted = reserved_bash_admitted_declarations(&config, &provider, view, selection);
    let admitted_names: HashSet<String> = admitted.iter().map(|tool| tool.name.clone()).collect();
    provider.ensure_unambiguous(&admitted_names)?;
    let client = config.nats_client(config.default_cluster_key()).await?;
    let hooks = tokio::time::timeout(
        Duration::from_secs(60),
        crate::nats_hook_provider::NatsHookProvider::discover_with_client(client, scope.clone()),
    )
    .await
    .context("Operator hook discovery timed out")??;
    let config = Arc::new(crate::config::ConfigLock::new(config));
    let slot = crate::bash_provider::BashEvalContextSlot::new();
    let selectors = view.use_tools.join(",");
    let mut params = BuildToolEvalContextParams::new(&config, scope)
        .with_agent_use_tools(Some(&selectors))
        .with_current_agent_package(view.package.clone())
        .with_command_bash(crate::bash_provider::CommandBashInputs {
            inner_declarations: admitted,
            // The operator already consented to this script's inner calls.
            operator_inner_consent: true,
        });
    params.bash_context_slot = Some(slot.clone());
    params.nats_hook_provider = Some(Arc::new(hooks));
    let mut ctx =
        crate::tool::build_tool_eval_context_with_provider(params, Arc::new(provider)).await;
    // Late overwrite, after the existing seams: the allowed names are the
    // synthetic `bash` plus the same admitted set the catalog and executor
    // allowlist derive from.
    ctx.allowed_tool_names = admitted_names;
    ctx.allowed_tool_names
        .insert(crate::config::BASH_TOOL_NAME.to_string());
    let root_call_id = uuid::Uuid::new_v4().to_string();
    super::approve_root_bash_call(&mut ctx, &root_call_id);
    let owner = slot.install(ctx);
    Ok(ReservedBashContext {
        owner,
        root_call_id,
    })
}

/// Evaluate one agentless Bash command on its slot-installed context (#2630).
/// The strong owner stays alive for the whole future, so the script provider
/// can upgrade the weak slot reference to fork inner-call contexts.
async fn evaluate_bash_owned(
    context: ReservedBashContext,
    invocation: super::BashInvocation<'_>,
) -> Result<OperatorToolReply> {
    let ReservedBashContext {
        owner,
        root_call_id,
    } = context;
    let reply = super::evaluate_bash(&owner, &root_call_id, invocation).await;
    drop(owner);
    reply
}

async fn reserved_eval_context(
    mut config: Config,
    view: &ToolReservationView,
    scope: &harnx_core::instance::ServerScope,
) -> Result<harnx_engine::tool::ToolEvalContext> {
    let provider = tokio::time::timeout(
        Duration::from_secs(60),
        NatsToolProvider::discover_strict(
            &config,
            scope.clone(),
            NatsInFlightCalls::default(),
            view.package.as_deref(),
        ),
    )
    .await
    .context("Tool discovery timed out")??;
    // Config clones share the cache. A reservation's catalog must remain private.
    config.nats_tool_declarations =
        Arc::new(parking_lot::RwLock::new(provider.declarations().to_vec()));
    let selected = selected_provider_names(&config, &provider, view);
    provider.ensure_unambiguous(&selected)?;
    let client = config.nats_client(config.default_cluster_key()).await?;
    let hooks = tokio::time::timeout(
        Duration::from_secs(60),
        crate::nats_hook_provider::NatsHookProvider::discover_with_client(client, scope.clone()),
    )
    .await
    .context("Operator hook discovery timed out")??;
    let config = Arc::new(crate::config::ConfigLock::new(config));
    let selectors = view.use_tools.join(",");
    let mut params = BuildToolEvalContextParams::new(&config, scope)
        .with_agent_use_tools(Some(&selectors))
        .with_current_agent_package(view.package.clone());
    params.nats_hook_provider = Some(Arc::new(hooks));
    let mut ctx =
        crate::tool::build_tool_eval_context_with_provider(params, Arc::new(provider)).await;
    ctx.allowed_tool_names = selected;
    Ok(ctx)
}

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
    let ToolCommandExecution {
        command,
        json,
        abort,
    } = execution;
    let admitted = reservation.state();
    let scope = admitted
        .server_scope
        .clone()
        .context("Tool reservation is unavailable")?;
    let mut config = reservation.config().clone();
    if matches!(command, OperatorToolCommand::Call { .. }) && config.run_context.is_none() {
        config.run_context = Some(reservation.admit_external_call().await?);
    }
    let ctx = reserved_eval_context(config, view, &scope).await?;
    let mut state = reservation.subscribe();
    let observed = state.borrow_and_update().clone();
    ensure!(
        observed == admitted,
        "Tool reservation changed during discovery; command was not admitted"
    );
    ensure_current(reservation, &admitted)?;
    // Selection above already applies package sanitization and configured toolsets.
    let command = match command {
        OperatorToolCommand::List { .. } => OperatorToolCommand::List { pattern: None },
        _ => command.clone(),
    };
    let call = evaluate(ctx, &command, ToolOutputFormat::from_json_flag(json), abort);
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

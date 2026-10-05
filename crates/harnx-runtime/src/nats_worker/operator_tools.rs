//! Idle-session operator RPC. Reuses worker providers, server claims and hooks;
//! does not acquire an inference turn or append a user prompt.

use super::daemon_runtime::WorkerRuntime;
use super::hook_supervisor::{HookServerStartConfig, HookServerSupervisor};
use super::server_reconciler::ServerReconciler;
use crate::operator_tools::{OperatorToolMessage, OperatorToolReply, OperatorToolRequest};
use anyhow::{ensure, Context, Result};
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::task::AbortOnDropHandle;

const MAX_PENDING: usize = 32;

pub(super) async fn subscribe(runtime: Arc<WorkerRuntime>) -> Result<AbortOnDropHandle<()>> {
    let subject = crate::operator_tools::subject(&runtime.cluster, &runtime.activation_route)?;
    let mut requests = match runtime.activation_route {
        super::SessionActivationRoute::ClusterShared => {
            runtime
                .client
                .queue_subscribe(subject, "operator-tool-workers".into())
                .await?
        }
        _ => runtime.client.subscribe(subject).await?,
    };
    runtime.client.flush().await?;
    Ok(AbortOnDropHandle::new(tokio::spawn(async move {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = runtime.shutdown.cancelled() => break,
                Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(error) = result { log::warn!("operator tool task failed: {error}"); }
                }
                Some(message) = requests.next() => {
                    let Some(reply) = message.reply.clone() else { continue };
                    if tasks.len() >= MAX_PENDING {
                        send(&runtime.client, &reply, OperatorToolMessage::Finished(OperatorToolReply {
                            output: String::new(), error: Some("Worker is busy with operator tool commands".into()),
                        })).await;
                        continue;
                    }
                    let runtime = runtime.clone();
                    tasks.spawn(async move { handle(runtime, message).await });
                }
                else => break,
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    })))
}

async fn send(client: &async_nats::Client, reply: &str, message: OperatorToolMessage) {
    if let Ok(payload) = serde_json::to_vec(&message) {
        if let Err(error) = client.publish(reply.to_owned(), payload.into()).await {
            log::warn!("operator tool reply failed: {error}");
        }
    }
}

async fn handle(runtime: Arc<WorkerRuntime>, message: async_nats::Message) {
    let reply = message.reply.as_ref().expect("admitted reply").as_str();
    // Only a private reply inbox can carry the per-request cancellation signal.
    if !reply.starts_with("_INBOX.") || reply.contains(['*', '>']) {
        return;
    }
    let result = async {
        let request: OperatorToolRequest = serde_json::from_slice(&message.payload)?;
        ensure!(
            request.version == 1,
            "unsupported operator tool protocol version"
        );
        if let crate::operator_tools::OperatorToolCommand::Call { args_json, .. } = &request.command
        {
            crate::operator_tools::parse_arguments(args_json)?;
        }
        let mut cancellation = runtime.client.subscribe(format!("{reply}.cancel")).await?;
        runtime.client.flush().await?;
        let abort = crate::utils::create_abort_signal();
        let cancel_task = AbortOnDropHandle::new(tokio::spawn({
            let abort = abort.clone();
            let shutdown = runtime.shutdown.clone();
            async move {
                tokio::select! { _ = cancellation.next() => {}, _ = shutdown.cancelled() => {} }
                abort.set_ctrlc();
            }
        }));
        send(&runtime.client, reply, OperatorToolMessage::Started).await;
        let result =
            tokio::time::timeout(Duration::from_secs(600), execute(&runtime, request, &abort))
                .await
                .context("Operator tool command timed out; execution outcome may be unknown")?;
        drop(cancel_task);
        result
    }
    .await;
    let reply_value = result.unwrap_or_else(|error| OperatorToolReply {
        output: String::new(),
        error: Some(format!("{error:#}")),
    });
    send(
        &runtime.client,
        reply,
        OperatorToolMessage::Finished(reply_value),
    )
    .await;
}

struct Claim {
    reconciler: Option<Arc<ServerReconciler>>,
    token: String,
}
impl Claim {
    async fn close(&mut self) {
        if let Some(reconciler) = self.reconciler.take() {
            reconciler.release_users(&self.token).await;
            reconciler.sweep().await;
        }
    }
}
impl Drop for Claim {
    fn drop(&mut self) {
        if let Some(reconciler) = self.reconciler.take() {
            let token = self.token.clone();
            tokio::spawn(async move {
                reconciler.release_users(&token).await;
                reconciler.sweep().await;
            });
        }
    }
}

async fn execute(
    runtime: &WorkerRuntime,
    request: OperatorToolRequest,
    abort: &crate::utils::AbortSignal,
) -> Result<OperatorToolReply> {
    ensure!(!abort.aborted(), "Operator tool command cancelled");
    let record = runtime
        .session_metadata
        .get(&request.session_key)
        .await?
        .context("Active session metadata not found")?;
    record.metadata.validate_storage_key(&request.session_key)?;
    let config = Arc::new(crate::config::ConfigLock::new(
        runtime.config.read().clone(),
    ));
    super::daemon::install_session_metadata_agent(&config, &record.metadata)?;
    {
        let mut config = config.write();
        let mut session = crate::config::session::new(&config, &record.metadata.session_id, None)?;
        if let Some(selectors) = &request.use_tools {
            session.set_use_tools(Some(selectors.clone()));
        }
        // The session metadata tools reach the session through its sink. A
        // direct call holds no lease, so it writes unfenced, as a frontend's
        // dot commands do; nothing on this path appends to the log.
        let sink: Arc<dyn crate::config::session::SessionAppendSink> = Arc::new(
            super::NatsSessionLogBackend::new(
                runtime.jetstream.clone(),
                request.session_key.clone(),
                runtime.lease.replicas,
            )
            .with_metadata_store(Some(runtime.session_metadata.clone())),
        );
        session.runtime = Some(Arc::new(sink));
        config.session = Some(session);
        // Isolate declaration caches too; Config::clone shares its cache Arc.
        config.nats_tool_declarations = Arc::new(parking_lot::RwLock::new(Vec::new()));
        config.maintenance_abort = Some(abort.clone());
    }
    admit_operator_call(runtime, &config, &request).await?;
    let mut claim = Claim {
        reconciler: runtime.server_reconciler.clone(),
        token: format!("operator-tool:{}", uuid::Uuid::new_v4()),
    };
    let result = execute_claimed(
        OperatorExecution {
            runtime,
            config: &config,
            request: &request,
            abort,
        },
        &claim,
    )
    .await;
    claim.close().await;
    result
}

async fn admit_operator_call(
    runtime: &WorkerRuntime,
    config: &crate::config::GlobalConfig,
    request: &OperatorToolRequest,
) -> Result<()> {
    use crate::nats_session_metadata::{CallTimeoutOverride, RunLimitsRecord};
    let snapshot = config.read().clone();
    if !matches!(
        request.command,
        crate::operator_tools::OperatorToolCommand::Call { .. }
    ) || snapshot.run_context.is_some()
    {
        return Ok(());
    }
    // Only this explicit operator CALL is a frontend boundary. Keep any inherited
    // run frozen, and don't turn inspection into execution admission.
    let record = RunLimitsRecord::admit_root(
        Default::default(),
        Default::default(),
        chrono::Utc::now(),
        snapshot.data.run_limits,
        snapshot.agent.as_deref(),
        CallTimeoutOverride::Omitted,
    )?;
    runtime
        .session_metadata
        .put_run_limits(&request.session_key, &record)
        .await?;
    runtime
        .session_metadata
        .put_invocation_limits(&request.session_key, &record)
        .await?;
    config.write().run_context = Some(record);
    Ok(())
}

struct OperatorExecution<'a> {
    runtime: &'a WorkerRuntime,
    config: &'a crate::config::GlobalConfig,
    request: &'a OperatorToolRequest,
    abort: &'a crate::utils::AbortSignal,
}

async fn execute_claimed(
    execution: OperatorExecution<'_>,
    claim: &Claim,
) -> Result<OperatorToolReply> {
    let OperatorExecution {
        runtime,
        config,
        request,
        abort,
    } = execution;
    tokio::select! {
        _ = super::daemon_background::await_initial_background_services(&runtime.background_services_attempted) => {},
        _ = crate::utils::wait_abort_signal(abort) => anyhow::bail!("Operator tool command cancelled"),
    }
    let selectors = if request.tool_use && config.read().tool_use {
        request
            .use_tools
            .clone()
            .or_else(|| config.read().extract_agent().use_tools())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let package = config.read().active_package();
    let servers = super::daemon_background::tool_servers_for_view(
        &config.read(),
        package.as_deref(),
        &selectors,
    );
    if let Some(reconciler) = &claim.reconciler {
        let claimed = reconciler.claim_users(&claim.token, servers).await;
        WorkerRuntime::wait_for_tool_server_start(reconciler.clone(), claimed, &claim.token).await;
    }
    // Invocation-owned agent hooks use a separate scope, so nested agents cannot
    // discover their parent's policies. Stop them before releasing tool claims.
    let mut hooks = if matches!(
        request.command,
        crate::operator_tools::OperatorToolCommand::Call { .. }
    ) {
        start_agent_hooks(runtime, config, &claim.token).await?
    } else {
        None
    };
    let result = execution
        .execute_discovered(&selectors, package, hooks.as_ref().map(|(_, scope)| scope))
        .await;
    if let Some((hooks, _)) = &mut hooks {
        hooks.shutdown().await;
    }
    result
}

impl OperatorExecution<'_> {
    async fn execute_discovered(
        &self,
        selectors: &[String],
        package: Option<String>,
        hook_scope: Option<&harnx_core::instance::ServerScope>,
    ) -> Result<OperatorToolReply> {
        let Self {
            runtime,
            config,
            request,
            abort,
        } = *self;
        let use_tools = selectors.join(",");
        let params =
            crate::tool_context::BuildToolEvalContextParams::new(config, &runtime.instance_id)
                .with_agent_use_tools(Some(&use_tools))
                .with_current_agent_package(package);
        // Fresh discovery prevents stale registrations from previous commands.
        let snapshot = config.read().clone();
        let provider = crate::nats_tool_provider::NatsToolProvider::discover_strict(
            &snapshot,
            runtime.instance_id.clone(),
            Default::default(),
            snapshot.active_package().as_deref(),
        )
        .await
        .context("Operator tool discovery failed")?;
        *config.read().nats_tool_declarations.write() = provider.declarations().to_vec();
        let hook_provider = self.discover_hooks(hook_scope).await?;
        let params = crate::tool_context::BuildToolEvalContextParams {
            nats_hook_provider: Some(hook_provider),
            ..params
        };
        let provider = Arc::new(provider);
        let mut ctx =
            crate::tool::build_tool_eval_context_with_provider(params, provider.clone()).await;
        // The declaration cache is an inventory, not an allowlist. In particular,
        // injecting a strict snapshot must not expose an agent's hidden tools.
        ctx.allowed_tool_names = config
            .read()
            .select_tools_for_package(selectors, snapshot.active_package().as_deref())
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        let command = self.filtered_command(&mut ctx, snapshot.active_package().as_deref());
        let names = match &command {
            crate::operator_tools::OperatorToolCommand::Info { name }
            | crate::operator_tools::OperatorToolCommand::Call { name, .. } => {
                std::collections::HashSet::from([name.clone()])
            }
            _ => ctx.allowed_tool_names.clone(),
        };
        provider.ensure_unambiguous(&names)?;
        crate::operator_tools::evaluate(
            ctx,
            &command,
            crate::tool_output::ToolOutputFormat::from_json_flag(request.json),
            abort,
        )
        .await
    }

    async fn discover_hooks(
        &self,
        hook_scope: Option<&harnx_core::instance::ServerScope>,
    ) -> Result<Arc<crate::nats_hook_provider::NatsHookProvider>> {
        let runtime = self.runtime;
        let mut hook_provider = crate::nats_hook_provider::NatsHookProvider::discover_with_client(
            runtime.client.clone(),
            runtime.instance_id.clone(),
        )
        .await
        .context("Operator hook provider discovery failed")?;
        if let Some(scope) = hook_scope {
            let own_hooks = crate::nats_hook_provider::NatsHookProvider::discover_with_client(
                runtime.client.clone(),
                scope.clone(),
            )
            .await?;
            hook_provider = hook_provider.with_scoped_hooks(own_hooks);
        }
        Ok(Arc::new(hook_provider))
    }

    fn filtered_command(
        &self,
        ctx: &mut harnx_engine::tool::ToolEvalContext,
        package: Option<&str>,
    ) -> crate::operator_tools::OperatorToolCommand {
        match &self.request.command {
            crate::operator_tools::OperatorToolCommand::List {
                pattern: Some(pattern),
            } => {
                let selected: std::collections::HashSet<_> = self
                    .config
                    .read()
                    .select_tools_for_package(std::slice::from_ref(pattern), package)
                    .into_iter()
                    .map(|tool| tool.name)
                    .collect();
                ctx.allowed_tool_names
                    .retain(|name| selected.contains(name));
                crate::operator_tools::OperatorToolCommand::List { pattern: None }
            }
            command => command.clone(),
        }
    }
}

async fn start_agent_hooks(
    runtime: &WorkerRuntime,
    config: &crate::config::GlobalConfig,
    label: &str,
) -> Result<Option<(HookServerSupervisor, harnx_core::instance::ServerScope)>> {
    let hooks = config
        .read()
        .agent
        .as_ref()
        .and_then(|agent| agent.hooks().cloned())
        .unwrap_or_default();
    if hooks.entries.is_empty() {
        return Ok(None);
    }
    ensure!(
        runtime.manage_servers,
        "Worker cannot start the active agent's operator hooks"
    );
    let snapshot = config.read().clone();
    let server = snapshot
        .resolve_nats_server(snapshot.default_cluster_key())
        .await?;
    let token = server
        .token
        .clone()
        .context("Operator agent hooks require a NATS token")?;
    let scope = harnx_core::instance::ServerScope::new();
    let start = HookServerStartConfig::new(
        runtime.client.clone(),
        scope.clone(),
        server.url.clone(),
        token,
    )
    .with_replicas(server.replicas)
    .with_broker_settings(&harnx_nats_common::connect::NatsEndpoint::from(
        server.as_ref(),
    ));
    Ok(Some((
        HookServerSupervisor::start_local(start, &hooks, label).await?,
        scope,
    )))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "operator_tools/admission_tests.rs"]
mod admission_tests;

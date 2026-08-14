//! Agent loop entrypoint for NATS-backed sessions.

use super::backend::{FencedSessionLogSink, NatsSessionLogBackend};
use super::hook_supervisor::{HookServerStartConfig, HookServerSupervisor};
use crate::agent_loop::OnToolRoundFn;
use crate::config::{resolve_local_nats_server_config, GlobalConfig, Input};
use crate::nats_event_sink::NatsEventSink;
use crate::nats_hook_provider::{
    dispatch_hook_event, HookDispatchMeta, HookEventDispatch, NatsHookProvider,
};
use crate::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
use crate::nats_metrics;
use crate::nats_session_index::{put_record, SessionIndexRecord};
use crate::tool_context::discover_nats_hook_provider_cached;
use crate::utils::AbortSignal;
use anyhow::{Context, Result};
use async_nats::jetstream;
use harnx_core::message::Message;
use harnx_core::session::SessionLogEntry;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct RunAgentLoopArgs<'a> {
    pub cluster_key: &'a str,
    /// Whether this worker launches its own agent-level hook servers (session
    /// and handoff hooks) rather than discovering independently deployed ones.
    pub manage_servers: bool,
    pub session_id: &'a str,
    pub config: GlobalConfig,
    pub instance_id: harnx_core::instance::ServerScope,
    pub initial_input: Input,
    pub abort_signal: AbortSignal,
    pub call_fn: Option<crate::agent_loop::AgentCallFn>,
    pub lease: Option<Arc<NatsSessionLease>>,
    pub lease_config: NatsLeaseConfig,
    pub after_seq_observer: Option<Arc<AtomicU64>>,
    /// Optional observer of the JetStream seq of the header-insert migration
    /// EditEntries (S2), when the worker migrates a headerless remote session on
    /// this activation. The migration re-maps the leading-user block onto this
    /// seq, so the daemon must advance its activation high-water cursor to cover
    /// it — otherwise the end-of-turn drain re-folds the now-remapped (and
    /// already-answered) leading user messages and re-runs the turn (S3).
    pub header_insert_observer: Option<Arc<AtomicU64>>,
    /// Optional NATS KV store for the session index. When set, the worker
    /// upserts a `SessionIndexRecord` after the effective log has a `Header`
    /// on this activation, so remote sessions taking the existing-session path
    /// (headerless-migrated per S2, or normal resumes) are indexed and their
    /// `last_activity` is refreshed — not just brand-new empty-log sessions.
    pub session_index: Option<&'a async_nats::jetstream::kv::Store>,
    pub on_tool_round: Option<OnToolRoundFn>,
    pub working_dir: Option<std::path::PathBuf>,
}

impl<'a> RunAgentLoopArgs<'a> {
    pub fn with_lease(mut self, lease: Arc<NatsSessionLease>) -> Self {
        self.lease = Some(lease);
        self
    }

    pub fn with_after_seq_observer(mut self, observer: Arc<AtomicU64>) -> Self {
        self.after_seq_observer = Some(observer);
        self
    }

    pub fn with_header_insert_observer(mut self, observer: Arc<AtomicU64>) -> Self {
        self.header_insert_observer = Some(observer);
        self
    }
}

fn fold_user_messages(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|message| message.content.to_text())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[allow(dead_code)]
pub(crate) fn last_fed_user_log_seq(_input: &Input) -> Option<u64> {
    // DEPRECATED: This function cannot meaningfully derive a cursor from Input.
    // The cursor MUST be derived from the log seq of messages that went into
    // the turn input. Callers should use the `seed_cursor` returned by
    // `derive_turn_input` or pass messages directly.
    //
    // Kept for compatibility with resumable path which passes a synthesized
    // Input from resumable_ctx.last_user, but the cursor must come from
    // `resumable_ctx.last_user.log_seq` directly at the call site.
    #[allow(dead_code)]
    None
}

pub(crate) fn fold_new_user_messages_since(
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
    cursor: Option<u64>,
) -> (Vec<Message>, Option<u64>) {
    // Apply mutations first: retracted/edited entries must be filtered.
    let effective_entries = match harnx_core::session_reconstruct::apply_log_mutations_nats(entries)
    {
        Ok(entries) => entries,
        Err(err) => {
            log::warn!("failed to apply NATS log mutations while folding user messages: {err}");
            return (Vec::new(), cursor);
        }
    };

    let mut messages = Vec::new();
    let mut latest_seq = cursor;
    for (seq, entry) in effective_entries {
        // Entry may be mutated; cursor semantics still skip seq <= cursor.
        if cursor.is_some_and(|seen| seq <= seen) {
            continue;
        }
        if let harnx_core::session::SessionLogEntry::Message {
            role,
            content,
            timestamp,
            ..
        } = entry
        {
            if role.is_user() {
                messages.push(
                    Message::new(role, content)
                        .with_log_seq(usize::try_from(seq).expect("JetStream seq fits usize"))
                        .with_log_timestamp(timestamp.unwrap_or_else(chrono::Utc::now)),
                );
                latest_seq = Some(seq);
            }
        }
    }
    (messages, latest_seq)
}

pub(crate) fn build_mid_turn_injection_callback(
    backend: NatsSessionLogBackend,
    cursor: Arc<AtomicU64>,
) -> OnToolRoundFn {
    Arc::new(move |merged_input, _results| {
        let backend = backend.clone();
        let cursor = Arc::clone(&cursor);
        Box::pin(async move {
            let tail = match backend.load_events_latest_async().await {
                Ok(entries) => entries,
                Err(err) => {
                    log::warn!("failed to reload session log for mid-turn injection: {err}");
                    return;
                }
            };
            let current = match cursor.load(std::sync::atomic::Ordering::SeqCst) {
                0 => None,
                seq => Some(seq),
            };
            let (messages, latest_seq) = fold_new_user_messages_since(&tail, current);
            if messages.is_empty() {
                return;
            }
            merged_input.set_injected_user_text(fold_user_messages(&messages));
            if let Some(seq) = latest_seq {
                cursor.store(seq, std::sync::atomic::Ordering::SeqCst);
            }
        })
    })
}

struct RepairOrphanToolCallsArgs<'a> {
    config: GlobalConfig,
    instance_id: &'a harnx_core::instance::ServerScope,
    fence_token: Option<u64>,
    worker_id: Option<String>,
    session_id: &'a str,
    abort_signal: &'a AbortSignal,
}

/// Run the agent loop with a remote NATS session.
///
/// Connects to the given cluster, loads/replays the session from JetStream,
/// then runs `run_agent_loop` with persistence redirected to NATS.
///
/// For P1.3: single worker assumed sole owner (no HA/lease).
pub async fn run_agent_loop_with_nats(args: RunAgentLoopArgs<'_>) -> Result<()> {
    run_agent_loop_with_nats_inner(args).await
}

/// Like [`run_agent_loop_with_nats`], but fence-guarded by the holding lease
/// (P2.2). When `lease` is `Some`, every worker-originated append is gated on
/// `lease.is_held()` and stamped with the lease fence, and the session resume
/// is aborted if the persisted log tail already carries a fence GREATER than
/// the lease revision this worker holds (a newer worker has taken over).
pub async fn run_agent_loop_with_nats_inner(args: RunAgentLoopArgs<'_>) -> Result<()> {
    let RunAgentLoopArgs {
        cluster_key,
        manage_servers,
        session_id,
        config,
        instance_id,
        initial_input,
        abort_signal,
        call_fn,
        lease,
        lease_config,
        after_seq_observer,
        header_insert_observer,
        session_index,
        on_tool_round,
        working_dir,
    } = args;
    let (jetstream_ctx, session_origin) = prepare_agent_session(PrepareAgentSessionParams {
        cluster_key,
        session_id,
        config: &config,
        instance_id: &instance_id,
        initial_input: &initial_input,
        abort_signal: &abort_signal,
        lease: lease.as_ref(),
        after_seq_observer,
        header_insert_observer: header_insert_observer.as_ref(),
        session_index,
        working_dir: working_dir.as_deref(),
    })
    .await?;

    let hook_start_config =
        resolve_agent_hook_start_config(manage_servers, &config, &jetstream_ctx, &instance_id)
            .await;
    let mut hook_supervisor = None;
    reconcile_agent_hooks(
        &mut hook_supervisor,
        hook_start_config.as_ref(),
        &config,
        session_id,
    )
    .await;

    let ctx = build_agent_loop_context(AgentContextParams {
        config: config.clone(),
        instance_id,
        abort_signal: abort_signal.clone(),
        call_fn,
        on_tool_round,
        working_dir,
    })
    .await;

    // After hook reconciliation and provider discovery so the dispatch reaches
    // both global and agent hook servers.
    dispatch_context_session_start(&ctx, session_origin, session_id).await;

    // Run unified agent loop
    // Persistence goes through shared Config.save_message entry construction; append_event routes sink
    run_agent_loop_segment(AgentLoopSegmentArgs {
        manage_servers,
        config,
        ctx,
        input: initial_input,
        abort_signal,
        jetstream_ctx,
        lease,
        lease_config,
        session_index: session_index.cloned(),
        hook_start_config,
        hook_supervisor,
    })
    .await
}

struct PrepareAgentSessionParams<'a> {
    cluster_key: &'a str,
    session_id: &'a str,
    config: &'a GlobalConfig,
    instance_id: &'a harnx_core::instance::ServerScope,
    initial_input: &'a Input,
    abort_signal: &'a AbortSignal,
    lease: Option<&'a Arc<NatsSessionLease>>,
    after_seq_observer: Option<Arc<AtomicU64>>,
    header_insert_observer: Option<&'a Arc<AtomicU64>>,
    session_index: Option<&'a async_nats::jetstream::kv::Store>,
    working_dir: Option<&'a std::path::Path>,
}

async fn prepare_agent_session(
    params: PrepareAgentSessionParams<'_>,
) -> Result<(jetstream::Context, SessionOrigin)> {
    let cfg_snapshot = params.config.read().clone();
    let jetstream = cfg_snapshot.nats_jetstream(params.cluster_key).await?;
    let mut backend = NatsSessionLogBackend::new(jetstream.clone(), params.session_id);
    if let Some(observer) = params.after_seq_observer {
        backend = backend.with_after_seq_observer(observer);
    }
    abort_resume_if_fenced(&backend, params.lease.map(Arc::as_ref))?;
    let (session, origin) = load_or_repair_session(LoadOrRepairSessionParams {
        backend: &backend,
        config: params.config,
        instance_id: params.instance_id,
        input: params.initial_input,
        lease: params.lease.map(Arc::as_ref),
        session_index: params.session_index,
        session_id: params.session_id,
        working_dir: params.working_dir,
        abort_signal: params.abort_signal,
        header_insert_observer: params.header_insert_observer,
    })
    .await?;
    attach_session_to_config(params.config, session, &backend, params.lease);
    Ok((jetstream, origin))
}

struct AgentContextParams {
    config: GlobalConfig,
    instance_id: harnx_core::instance::ServerScope,
    abort_signal: AbortSignal,
    call_fn: Option<crate::agent_loop::AgentCallFn>,
    on_tool_round: Option<OnToolRoundFn>,
    working_dir: Option<std::path::PathBuf>,
}

async fn build_agent_loop_context(
    params: AgentContextParams,
) -> crate::agent_loop::AgentLoopContext {
    let config_snapshot = params.config.read().clone();
    let nats_hook_provider =
        discover_nats_hook_provider_cached(&config_snapshot, &params.instance_id).await;
    crate::agent_loop::AgentLoopContext {
        config: params.config,
        instance_id: params.instance_id,
        abort_signal: params.abort_signal,
        call_fn: params.call_fn,
        on_tool_round: params.on_tool_round,
        on_text_response: None,
        initial_with_embeddings: false,
        initial_resume_count: 0,
        max_resume: None,
        nats_hook_provider,
        pending_async_context: Some(Arc::new(tokio::sync::Mutex::new(None))),
        working_dir: params.working_dir,
    }
}

/// Whether this activation created the session or picked up an existing one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionOrigin {
    Created,
    Resumed,
}

struct SessionStartDispatch<'a> {
    origin: SessionOrigin,
    provider: Option<&'a NatsHookProvider>,
    session_id: &'a str,
    cwd: std::path::PathBuf,
    model: String,
    pending_async_context: Option<Arc<tokio::sync::Mutex<Option<String>>>>,
}

/// Dispatch SessionStart using the loop context's provider, model, and cwd.
///
/// Any additional context the hooks return rides the loop's pending queue into
/// the first turn.
async fn dispatch_context_session_start(
    ctx: &crate::agent_loop::AgentLoopContext,
    origin: SessionOrigin,
    session_id: &str,
) {
    // Same cwd rule as the shared agent loop: the session's working directory
    // when it has one, else the worker's.
    let cwd = ctx
        .working_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let model = ctx.config.read().current_model().id().to_string();
    dispatch_session_start(SessionStartDispatch {
        origin,
        provider: ctx.nats_hook_provider.as_deref(),
        session_id,
        cwd,
        model,
        pending_async_context: ctx.pending_async_context.clone(),
    })
    .await;
}

/// Fire SessionStart for a session this worker just created.
///
/// The worker owns this event because only it can reach the hook servers it
/// launched. Every activation of an existing session skips it: activations
/// happen once per turn, and the activation that created the session already
/// fired it.
///
/// The outcome is dropped on purpose. SessionStart hooks observe and contribute
/// context; the session already exists by the time they run, so there is nothing
/// for a `Block` to prevent.
async fn dispatch_session_start(params: SessionStartDispatch<'_>) {
    if params.origin == SessionOrigin::Resumed {
        return;
    }
    let _ = dispatch_hook_event(HookEventDispatch {
        event: harnx_core::hooks::HookEvent::SessionStart {
            source: "startup".to_string(),
            model: params.model,
        },
        provider: params.provider,
        meta: HookDispatchMeta {
            session_id: params.session_id.to_string(),
            cwd: params.cwd,
            resume_count: 0,
        },
        pending_async_context: params.pending_async_context,
    })
    .await;
}

struct AgentLoopSegmentArgs {
    /// Carried through handoffs so a handoff target's hooks can be
    /// re-resolved with the same manage-vs-discover gate the activation used
    /// (see `prepare_nats_handoff`); a worker that discovers independently
    /// deployed servers must not start local supervisors at handoff either.
    manage_servers: bool,
    config: GlobalConfig,
    ctx: crate::agent_loop::AgentLoopContext,
    input: Input,
    abort_signal: AbortSignal,
    jetstream_ctx: jetstream::Context,
    lease: Option<Arc<NatsSessionLease>>,
    lease_config: NatsLeaseConfig,
    session_index: Option<async_nats::jetstream::kv::Store>,
    hook_start_config: Option<HookServerStartConfig>,
    hook_supervisor: Option<HookServerSupervisor>,
}

/// Resolve the active agent's hooks and hand them to [`agent_hook_start_config`].
///
/// Split out of `run_agent_loop_with_nats_inner` to keep that function under
/// the line-count threshold; `reconcile_agent_hooks` re-resolves the same
/// hooks a few lines later (a config read, not worth threading through).
async fn resolve_agent_hook_start_config(
    manage_servers: bool,
    config: &GlobalConfig,
    jetstream: &jetstream::Context,
    instance_id: &harnx_core::instance::ServerScope,
) -> Option<HookServerStartConfig> {
    let hooks = agent_resolved_hooks(config);
    agent_hook_start_config(manage_servers, &hooks, jetstream, instance_id).await
}

async fn agent_hook_start_config(
    manage_servers: bool,
    hooks: &harnx_core::hooks::HooksConfig,
    jetstream: &jetstream::Context,
    instance_id: &harnx_core::instance::ServerScope,
) -> Option<HookServerStartConfig> {
    // This runs once per activation, so unlike the worker-startup gates it
    // pays for a local NATS server resolution (and, absent a broker address,
    // a shared-server startup) on every turn unless we also check that the
    // active agent actually has hooks to launch — mirrors `start_global_hooks`.
    if !manage_servers || hooks.entries.is_empty() {
        return None;
    }
    let result = async {
        let server = resolve_local_nats_server_config().await?;
        // Read before `server.token` moves out below: `NatsEndpoint::from`
        // borrows the whole config, which a partial move would then forbid.
        let tls_endpoint = harnx_nats_common::connect::NatsEndpoint::from(&server);
        let token = server
            .token
            .context("local NATS agent hooks require HARNX_NATS_TOKEN")?;
        Result::<_>::Ok(
            HookServerStartConfig::new(
                jetstream.client().clone(),
                instance_id.clone(),
                server.url,
                token,
            )
            .with_replicas(server.replicas)
            .with_tls(&tls_endpoint),
        )
    }
    .await;
    match result {
        Ok(config) => Some(config),
        Err(error) => {
            log::warn!("session NATS hook servers disabled: {error:#}");
            None
        }
    }
}

fn run_agent_loop_segment(
    args: AgentLoopSegmentArgs,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> {
    Box::pin(async move {
        let AgentLoopSegmentArgs {
            manage_servers,
            config,
            ctx,
            input,
            abort_signal,
            jetstream_ctx,
            lease,
            lease_config,
            session_index,
            hook_start_config,
            hook_supervisor,
        } = args;
        let loop_result = crate::agent_loop::run_agent_loop(&ctx, input).await?;
        match loop_result {
            crate::agent_loop::LoopResult::Completed => Ok(()),
            crate::agent_loop::LoopResult::HandoffRequested {
                agent,
                session_id,
                prompt,
            } => {
                let handoff_input = crate::config::input::from_str(&config, &prompt, None);
                let handoff_args = AgentLoopSegmentArgs {
                    manage_servers,
                    config,
                    ctx,
                    input: handoff_input,
                    abort_signal,
                    jetstream_ctx,
                    lease,
                    lease_config,
                    session_index,
                    hook_start_config,
                    hook_supervisor,
                };
                let (handoff_args, new_event_sink) =
                    prepare_nats_handoff(handoff_args, agent, session_id).await?;
                harnx_core::sink::with_agent_event_sink(new_event_sink, async move {
                    run_agent_loop_segment(handoff_args).await
                })
                .await
            }
        }
    })
}

async fn prepare_nats_handoff(
    mut args: AgentLoopSegmentArgs,
    agent: String,
    session_id: Option<String>,
) -> Result<(AgentLoopSegmentArgs, Arc<NatsEventSink>)> {
    let previous_lease = args
        .lease
        .as_ref()
        .context("NATS handoff requires active session lease")?;
    args.config.write().exit_agent()?;
    crate::config::Config::use_agent(
        &args.config,
        &agent,
        session_id.as_deref(),
        args.abort_signal.clone(),
    )
    .await?;
    let new_session_id = args
        .config
        .read()
        .session
        .as_ref()
        .and_then(|session| session.session_id.clone())
        .context("NATS handoff did not establish new session")?;
    let new_lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream: args.jetstream_ctx.clone(),
        session_id: &new_session_id,
        worker_id: previous_lease.worker_id().to_string(),
        generation: previous_lease.generation(),
        config: args.lease_config.clone(),
        session_index: args.session_index.clone(),
    })
    .await?
    .with_context(|| {
        format!("Failed to acquire NATS lease for handed-off session '{new_session_id}'")
    })?;
    let new_lease = Arc::new(new_lease);
    let new_event_sink = Arc::new(
        NatsEventSink::new(
            args.jetstream_ctx.client().clone(),
            args.jetstream_ctx.clone(),
            new_session_id.clone(),
        )
        .await,
    );
    let new_after_seq_observer = new_event_sink.after_seq_handle();
    let mut new_backend = NatsSessionLogBackend::new(args.jetstream_ctx.clone(), &new_session_id);
    new_backend = new_backend.with_after_seq_observer(Arc::clone(&new_after_seq_observer));
    let new_session = args
        .config
        .read()
        .session
        .clone()
        .context("NATS handoff missing session after activation")?;
    attach_session_to_config(&args.config, new_session, &new_backend, Some(&new_lease));
    args.lease = Some(new_lease);
    reconcile_handoff_target_hooks(&mut args, &new_session_id).await;
    Ok((args, new_event_sink))
}

/// Re-resolve `hook_start_config` against the handoff target's hooks, then
/// reconcile the supervisor against it.
///
/// `hook_start_config` was resolved once at activation, from the activation
/// agent's own hooks (or lack of them). Reusing it unchanged here means a
/// handoff to an agent WITH hooks that the activation agent lacked never
/// starts them: `reconcile_hook_supervisor` just no-ops on a `None` start.
/// `use_agent` (above, in `prepare_nats_handoff`) has already switched
/// `args.config` to the target agent by the time this runs, so re-resolving
/// against it reflects the target's hooks instead. `resolve_agent_hook_start_config`
/// still returns `None` without ever touching the broker when the target has
/// no hooks either, so a hookless handoff costs nothing extra.
async fn reconcile_handoff_target_hooks(args: &mut AgentLoopSegmentArgs, new_session_id: &str) {
    args.hook_start_config = resolve_agent_hook_start_config(
        args.manage_servers,
        &args.config,
        &args.jetstream_ctx,
        &args.ctx.instance_id,
    )
    .await;
    reconcile_agent_hooks(
        &mut args.hook_supervisor,
        args.hook_start_config.as_ref(),
        &args.config,
        new_session_id,
    )
    .await;
}

fn agent_resolved_hooks(config: &GlobalConfig) -> harnx_core::hooks::HooksConfig {
    config
        .read()
        .agent
        .as_ref()
        .and_then(|agent| agent.hooks().cloned())
        .unwrap_or_default()
}

async fn reconcile_agent_hooks(
    current: &mut Option<HookServerSupervisor>,
    start: Option<&HookServerStartConfig>,
    config: &GlobalConfig,
    session_id: &str,
) {
    let hooks = agent_resolved_hooks(config);
    let scope = format!("session-{session_id}");
    reconcile_hook_supervisor(current, start, &hooks, &scope).await;
}

/// Replace one session's hook processes only after its previous registrations
/// have been removed. Public for lifecycle integration coverage.
#[doc(hidden)]
pub async fn reconcile_hook_supervisor(
    current: &mut Option<HookServerSupervisor>,
    start: Option<&HookServerStartConfig>,
    hooks: &harnx_core::hooks::HooksConfig,
    scope: &str,
) {
    // Stop first so old registrations and processes are gone before new hooks register.
    if let Some(mut previous) = current.take() {
        previous.shutdown().await;
    }
    let Some(start) = start else {
        return;
    };
    if hooks.entries.is_empty() {
        return;
    }
    match HookServerSupervisor::start_local(start.clone(), hooks, scope).await {
        Ok(supervisor) => *current = Some(supervisor),
        Err(error) => {
            // Failures happen before the supervisor can own cleanup or while its KV
            // route is unavailable. Publishing here would either reuse the failed
            // route or leave an unowned rejector after the session ends. Registry
            // read failures are guarded by NatsHookProvider instead.
            log::warn!("session NATS hook servers disabled: {error:#}");
        }
    }
}

/// Fence-on-resume fail-safe: if the persisted tail carries a worker fence
fn abort_resume_if_fenced(
    backend: &NatsSessionLogBackend,
    lease: Option<&NatsSessionLease>,
) -> Result<()> {
    let Some(lease) = lease else {
        return Ok(());
    };
    let entries: Vec<harnx_core::session::SessionLogEntry> = backend
        .load_events_blocking()?
        .into_iter()
        .map(|(_, e)| e)
        .collect();
    if let Some(max_fence) = harnx_core::session::max_worker_fence_token(&entries) {
        if max_fence > lease.fence_token() {
            anyhow::bail!(
                "aborting resume: log tail fence {max_fence} exceeds held lease revision {} (fenced by a newer worker)",
                lease.fence_token()
            );
        }
    }
    Ok(())
}

/// Parameters for [`load_or_repair_session`].
struct LoadOrRepairSessionParams<'a> {
    backend: &'a NatsSessionLogBackend,
    config: &'a GlobalConfig,
    instance_id: &'a harnx_core::instance::ServerScope,
    input: &'a Input,
    lease: Option<&'a NatsSessionLease>,
    session_index: Option<&'a async_nats::jetstream::kv::Store>,
    session_id: &'a str,
    working_dir: Option<&'a std::path::Path>,
    abort_signal: &'a AbortSignal,
    /// When a header-insert migration is performed on this activation, its
    /// JetStream seq is published here so the daemon can advance its high-water
    /// cursor past the re-mapped leading-user block (S3).
    header_insert_observer: Option<&'a Arc<AtomicU64>>,
}

/// Load the session from the backend, creating a new one when the log is empty
/// or repairing orphan tool calls (with idempotency hints) when resuming an
/// existing session.
async fn load_or_repair_session(
    params: LoadOrRepairSessionParams<'_>,
) -> Result<(harnx_core::session::Session, SessionOrigin)> {
    let LoadOrRepairSessionParams {
        backend,
        config,
        instance_id,
        input,
        lease,
        session_index,
        session_id,
        working_dir,
        abort_signal,
        header_insert_observer,
    } = params;
    let entries = backend.load_events_blocking()?;
    if entries.is_empty() {
        // New session: write header and load
        let session = write_header_and_load_session(
            backend,
            config,
            input,
            session_index,
            session_id,
            working_dir,
        )
        .await?;
        return Ok((session, SessionOrigin::Created));
    }

    let mut entries_vec = entries;
    let mut effective_entries =
        harnx_core::session_reconstruct::apply_log_mutations_nats(&entries_vec)?;
    // The thin client appends its user message before it activates a session, so
    // a brand-new session reaches this path with a headerless log rather than an
    // empty one. Writing the header here is what creates the session.
    let header_written = maybe_insert_remote_header(MaybeInsertRemoteHeaderArgs {
        backend,
        config,
        input,
        session_id,
        working_dir,
        header_insert_observer,
        entries_vec: &mut entries_vec,
        effective_entries: &mut effective_entries,
    })
    .await?;
    refresh_session_index_on_activation(session_index, &effective_entries, session_id).await;
    repair_orphan_tool_calls_if_any(RepairOrphanCallsParams {
        backend,
        config,
        instance_id,
        lease,
        session_id,
        abort_signal,
        effective_entries: &effective_entries,
        entries_vec: &mut entries_vec,
    })
    .await?;
    let session = crate::nats_session_log::load_session_from_entries(&entries_vec, session_id)?;
    let origin = if header_written {
        SessionOrigin::Created
    } else {
        SessionOrigin::Resumed
    };
    Ok((session, origin))
}

struct RepairOrphanCallsParams<'a> {
    backend: &'a NatsSessionLogBackend,
    config: &'a GlobalConfig,
    instance_id: &'a harnx_core::instance::ServerScope,
    lease: Option<&'a NatsSessionLease>,
    session_id: &'a str,
    abort_signal: &'a AbortSignal,
    effective_entries: &'a [(u64, SessionLogEntry)],
    entries_vec: &'a mut Vec<(u64, SessionLogEntry)>,
}

/// Repair tool calls left without results by a previous worker, reloading
/// `entries_vec` so the caller builds the session from the repaired log rather
/// than the stale snapshot that still holds the orphan calls. No-op when the
/// effective log has no orphans.
async fn repair_orphan_tool_calls_if_any(params: RepairOrphanCallsParams<'_>) -> Result<()> {
    let RepairOrphanCallsParams {
        backend,
        config,
        instance_id,
        lease,
        session_id,
        abort_signal,
        effective_entries,
        entries_vec,
    } = params;
    let orphan_calls = find_orphan_tool_calls(effective_entries);
    if orphan_calls.is_empty() {
        return Ok(());
    }
    nats_metrics::resume_detected();
    info!(
        "resume detected: session_id={} worker_id={} revision={} orphan_batches={}",
        session_id,
        lease.map(|l| l.worker_id()).unwrap_or("none"),
        lease.map(|l| l.fence_token()).unwrap_or(0),
        orphan_calls.len()
    );
    repair_orphan_tool_calls_with_hints(
        backend,
        &orphan_calls,
        RepairOrphanToolCallsArgs {
            config: config.clone(),
            instance_id,
            fence_token: lease.map(|l| l.fence_token()),
            worker_id: lease.map(|l| l.worker_id().to_string()),
            session_id,
            abort_signal,
        },
    )
    .await?;
    *entries_vec = backend.load_events_blocking()?;
    Ok(())
}

fn should_insert_remote_header(effective_entries: &[(u64, SessionLogEntry)]) -> bool {
    !effective_entries.iter().any(|(_, entry)| {
        matches!(
            entry,
            SessionLogEntry::Header { .. } | SessionLogEntry::Compress { .. }
        )
    })
}

fn build_remote_header_insert_replacements(
    raw_entries: &[(u64, SessionLogEntry)],
    effective_entries: &[(u64, SessionLogEntry)],
    config: &GlobalConfig,
    input: &Input,
    session_id: &str,
    working_dir: Option<&std::path::Path>,
) -> Result<Option<(usize, usize, Vec<String>)>> {
    if effective_entries.is_empty() || !should_insert_remote_header(effective_entries) {
        return Ok(None);
    }

    let mut first_user_seq = None;
    let mut last_user_seq = None;
    let mut replacements = Vec::new();
    replacements.push(serde_yaml::to_string(&build_remote_session_header(
        config,
        input,
        session_id,
        working_dir,
    )?)?);

    for (seq, entry) in raw_entries {
        match entry {
            SessionLogEntry::Message { role, .. } if role.is_user() => {
                let seq = usize::try_from(*seq)
                    .context("session log sequence does not fit into usize")?;
                if first_user_seq.is_none() {
                    first_user_seq = Some(seq);
                }
                last_user_seq = Some(seq);
                replacements.push(serde_yaml::to_string(entry)?);
            }
            _ if first_user_seq.is_some() => break,
            _ => return Ok(None),
        }
    }

    Ok(match (first_user_seq, last_user_seq) {
        (Some(from), Some(to)) => Some((from, to, replacements)),
        _ => None,
    })
}

fn remote_header_insert_message_id(session_id: &str, first_user_seq: usize) -> String {
    format!("{session_id}:header-insert:{first_user_seq}")
}

/// Attach the reconstructed session to the shared config with the NATS append
/// sink for the unified persistence path. With a lease, use the fence-guarded
/// sink so writes from a fenced-out worker are rejected.
fn attach_session_to_config(
    config: &GlobalConfig,
    mut session: harnx_core::session::Session,
    backend: &NatsSessionLogBackend,
    lease: Option<&Arc<NatsSessionLease>>,
) {
    let sink: Arc<dyn crate::config::session::SessionAppendSink> = match lease {
        Some(lease) => Arc::new(FencedSessionLogSink::new(
            backend.clone(),
            Arc::clone(lease),
        )),
        None => Arc::new(backend.clone()),
    };
    session.runtime = Some(Arc::new(sink));
    let mut cfg = config.write();
    cfg.session = Some(session);
}

/// Pending ToolCalls entry that lacks matching ToolResults.
#[allow(dead_code)]
struct PendingToolCalls {
    seq: u64,
    text: String,
    thought: Option<String>,
    calls: Vec<harnx_core::tool::ToolCall>,
    timestamp: Option<chrono::DateTime<chrono::Utc>>,
}

/// Decide whether an orphan tool call may be safely re-run during resume
/// repair. A tool is re-runnable when its declaration marks it idempotent or
/// read-only (MCP annotation hints). Unknown tools (absent from the map) are
/// treated as non-idempotent and must NOT be re-run.
pub(super) fn tool_can_rerun(
    decl_map: &std::collections::HashMap<String, harnx_core::tool::ToolDeclaration>,
    name: &str,
) -> bool {
    decl_map
        .get(name)
        .map(|d| d.idempotent_hint == Some(true) || d.read_only_hint == Some(true))
        .unwrap_or(false)
}

/// Find orphan tool calls in session log entries (trailing ToolCalls without matching ToolResults).
fn find_orphan_tool_calls(
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
) -> Vec<PendingToolCalls> {
    use harnx_core::session::SessionLogEntry;

    // Find the last ToolCalls entry that lacks a matching ToolResults
    let mut orphans = Vec::new();
    let mut last_tool_calls: Option<PendingToolCalls> = None;

    for (seq, entry) in entries {
        match entry {
            SessionLogEntry::ToolCalls {
                text,
                thought,
                calls,
                timestamp,
                ..
            } => {
                last_tool_calls = Some(PendingToolCalls {
                    seq: *seq,
                    text: text.clone(),
                    thought: thought.clone(),
                    calls: calls.clone(),
                    timestamp: *timestamp,
                });
            }
            SessionLogEntry::ToolResults { .. } => {
                // ToolResults clears the pending ToolCalls
                last_tool_calls = None;
            }
            SessionLogEntry::Message { role, .. } if role.is_user() => {
                // A new user message ends the prior turn; any orphan ToolCalls above it is still an orphan
                if let Some(tc) = last_tool_calls.take() {
                    orphans.push(tc);
                }
            }
            _ => {}
        }
    }

    // If we end with a pending ToolCalls, that's the orphan
    if let Some(tc) = last_tool_calls {
        orphans.push(tc);
    }

    orphans
}

/// Repair orphan tool calls: for each orphan, re-run idempotent/readonly tools,
/// synthesize interrupt-error for non-idempotent ones.
/// All ToolResults are appended via the backend (fence-stamped when lease is held).
async fn repair_orphan_tool_calls_with_hints(
    backend: &NatsSessionLogBackend,
    orphan_calls: &[PendingToolCalls],
    args: RepairOrphanToolCallsArgs<'_>,
) -> Result<()> {
    use harnx_core::session::SessionLogEntry;

    let tool_repair = build_tool_repair_context(&args.config);
    let eval_ctx =
        build_orphan_tool_eval_context(&args.config, args.instance_id, &tool_repair).await;

    for orphan in orphan_calls {
        let results = repair_single_orphan(orphan, &args, &tool_repair, &eval_ctx).await;
        let entry = apply_optional_fence_token(
            SessionLogEntry::ToolResults {
                results,
                timestamp: orphan.timestamp,
            },
            args.fence_token,
        );
        backend.append_event_blocking(&entry)?;
    }

    Ok(())
}

struct ToolRepairContext {
    decl_map: std::collections::HashMap<String, harnx_core::tool::ToolDeclaration>,
    agent_use_tools: Option<String>,
    current_agent_package: Option<String>,
}

fn build_tool_repair_context(config: &GlobalConfig) -> ToolRepairContext {
    let (decl_map, agent_use_tools, current_agent_package) = {
        let guard = config.read();
        let (tool_declarations, _) = guard.tool_declarations_for_use_tools(Some("*"), None);
        let decl_map = tool_declarations
            .into_iter()
            .map(|d| (d.name.clone(), d))
            .collect();
        let agent_use_tools = guard
            .agent
            .as_ref()
            .and_then(|a| a.use_tools().map(|v| v.join(",")));
        let current_agent_package = guard
            .agent
            .as_ref()
            .and_then(|a| harnx_core::package_namespace::pkg_from_qualified(a.name()))
            .map(str::to_string);
        (decl_map, agent_use_tools, current_agent_package)
    };

    ToolRepairContext {
        decl_map,
        agent_use_tools,
        current_agent_package,
    }
}

async fn build_orphan_tool_eval_context(
    config: &GlobalConfig,
    instance_id: &harnx_core::instance::ServerScope,
    repair: &ToolRepairContext,
) -> crate::tool::ToolEvalContext {
    crate::tool::build_tool_eval_context(crate::tool::BuildToolEvalContextParams {
        config,
        instance_id,
        agent_use_tools: repair.agent_use_tools.as_deref(),
        current_agent_package: repair.current_agent_package.clone(),
        working_dir: None,
        nats_hook_provider: None,
        pending_async_context: None,
    })
    .await
}

async fn repair_single_orphan(
    orphan: &PendingToolCalls,
    args: &RepairOrphanToolCallsArgs<'_>,
    repair: &ToolRepairContext,
    eval_ctx: &crate::tool::ToolEvalContext,
) -> Vec<harnx_core::session::ToolOutput> {
    let (mut results, rerun_calls) = partition_orphan_calls(orphan, args, repair);
    if !rerun_calls.is_empty() {
        let rerun_results =
            rerun_or_synthesize_tool_results(rerun_calls, eval_ctx, args.abort_signal).await;
        results.extend(rerun_results);
    }
    results
}

fn partition_orphan_calls(
    orphan: &PendingToolCalls,
    args: &RepairOrphanToolCallsArgs<'_>,
    repair: &ToolRepairContext,
) -> (
    Vec<harnx_core::session::ToolOutput>,
    Vec<harnx_core::tool::ToolCall>,
) {
    let mut results = Vec::new();
    let mut rerun_calls = Vec::new();

    for call in &orphan.calls {
        if tool_can_rerun(&repair.decl_map, &call.name) {
            log_resume_decision("rerun", args, call);
            rerun_calls.push(call.clone());
        } else {
            log_resume_decision("synthesize_interrupt_error", args, call);
            nats_metrics::interrupt_error_synthesized();
            results.push(interrupt_error_output(call));
        }
    }

    (results, rerun_calls)
}

async fn rerun_or_synthesize_tool_results(
    rerun_calls: Vec<harnx_core::tool::ToolCall>,
    eval_ctx: &crate::tool::ToolEvalContext,
    abort_signal: &AbortSignal,
) -> Vec<harnx_core::session::ToolOutput> {
    match crate::tool::eval_tool_calls(eval_ctx, rerun_calls.clone(), abort_signal).await {
        Ok(tool_results) => tool_results
            .into_iter()
            .map(|result| harnx_core::session::ToolOutput {
                id: result.call.id.clone(),
                name: result.call.name.clone(),
                output: result.output,
                markdown: result.markdown,
                content: result.content,
                switch_agent: result.switch_agent,
            })
            .collect(),
        Err(err) => rerun_calls
            .into_iter()
            .map(|call| rerun_failure_output(&call, &err))
            .collect(),
    }
}

fn apply_optional_fence_token(
    mut entry: harnx_core::session::SessionLogEntry,
    fence_token: Option<u64>,
) -> harnx_core::session::SessionLogEntry {
    if let Some(fence_token) = fence_token {
        entry.set_fence_token(fence_token);
    }
    entry
}

/// Log a per-call resume decision (`rerun` or `synthesize_interrupt_error`).
fn log_resume_decision(
    decision: &str,
    args: &RepairOrphanToolCallsArgs<'_>,
    call: &harnx_core::tool::ToolCall,
) {
    info!(
        "resume decision {decision}: session_id={} worker_id={} revision={} tool_name={} call_id={}",
        args.session_id,
        args.worker_id.as_deref().unwrap_or("none"),
        args.fence_token.unwrap_or(0),
        call.name,
        call.id.as_deref().unwrap_or("none")
    );
}

fn interrupt_error_output(call: &harnx_core::tool::ToolCall) -> harnx_core::session::ToolOutput {
    harnx_core::session::ToolOutput {
        id: call.id.clone(),
        name: call.name.clone(),
        output: serde_json::json!({
            "error": "tool response lost (session was interrupted before results were persisted)"
        }),
        markdown: None,
        content: Vec::new(),
        switch_agent: None,
    }
}

fn rerun_failure_output(
    call: &harnx_core::tool::ToolCall,
    err: &anyhow::Error,
) -> harnx_core::session::ToolOutput {
    harnx_core::session::ToolOutput {
        id: call.id.clone(),
        name: call.name.clone(),
        output: serde_json::json!({
            "error": format!("tool re-run failed: {err:#}")
        }),
        markdown: None,
        content: Vec::new(),
        switch_agent: None,
    }
}

fn build_remote_session_header(
    config: &GlobalConfig,
    input: &Input,
    session_id: &str,
    working_dir: Option<&std::path::Path>,
) -> Result<harnx_core::session::SessionLogEntry> {
    let mut header_session = crate::config::session::new(&config.read(), session_id, working_dir)?;
    header_session.set_agent(&input.agent)?;
    Ok(header_session.build_header_entry())
}

fn build_session_index_record_from_header(
    header: &harnx_core::session::SessionLogEntry,
    title: Option<String>,
) -> Result<SessionIndexRecord> {
    let harnx_core::session::SessionLogEntry::Header {
        session_id,
        agent_name,
        working_dir,
        git_branch,
        git_remote,
        ..
    } = header
    else {
        anyhow::bail!("remote session index requires header entry")
    };

    Ok(SessionIndexRecord {
        session_id: session_id
            .clone()
            .context("remote session header missing session_id")?,
        agent_name: agent_name
            .clone()
            .context("remote session header missing agent_name")?,
        working_dir: working_dir.clone(),
        git_branch: git_branch.clone(),
        git_remote: git_remote.clone(),
        title,
        last_activity: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock before unix epoch")?
            .as_secs(),
    })
}

/// Scan effective log entries for the most recent `Title` event so the session
/// index reflects the latest generated/manual title on activation upsert.
fn latest_title_from_entries(
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
) -> Option<String> {
    entries.iter().rev().find_map(|(_, entry)| match entry {
        harnx_core::session::SessionLogEntry::Title { title, .. } => Some(title.clone()),
        _ => None,
    })
}

async fn upsert_session_index_record(
    store: &async_nats::jetstream::kv::Store,
    header: &harnx_core::session::SessionLogEntry,
    title: Option<String>,
) -> Result<u64> {
    let record = build_session_index_record_from_header(header, title)?;
    put_record(store, &record)
        .await
        .with_context(|| format!("put session index record for {}", record.session_id))
}

/// Keep the session index current on every activation that takes the
/// existing-session path. Brand-new (empty-log) sessions register their index in
/// `write_header_and_load_session`; but headerless sessions migrated here (S2)
/// and normal resumes take THIS path, so we upsert from the effective `Header`
/// (carrying the latest `Title`) — otherwise the session never appears in
/// `list_remote_sessions_with_meta` (breaks resume/picker) and its
/// `last_activity` is never refreshed. Best-effort: warn on failure, never fail
/// activation. Idempotent (`put_record` upserts).
async fn refresh_session_index_on_activation(
    session_index: Option<&async_nats::jetstream::kv::Store>,
    effective_entries: &[(u64, SessionLogEntry)],
    session_id: &str,
) {
    let Some(store) = session_index else {
        return;
    };
    let Some((_, header)) = effective_entries
        .iter()
        .find(|(_, entry)| matches!(entry, SessionLogEntry::Header { .. }))
    else {
        return;
    };
    let title = latest_title_from_entries(effective_entries);
    if let Err(err) = upsert_session_index_record(store, header, title).await {
        log::warn!(
            "failed to upsert remote session index during activation: \
             session_id={session_id} err={err:#}"
        );
    }
}

struct MaybeInsertRemoteHeaderArgs<'a> {
    backend: &'a NatsSessionLogBackend,
    config: &'a GlobalConfig,
    input: &'a Input,
    session_id: &'a str,
    working_dir: Option<&'a std::path::Path>,
    header_insert_observer: Option<&'a Arc<AtomicU64>>,
    entries_vec: &'a mut Vec<(u64, SessionLogEntry)>,
    effective_entries: &'a mut Vec<(u64, SessionLogEntry)>,
}

/// Headerless sessions migrated in (S2) need a `Header` synthesized from the
/// leading user block. When required, this appends an `EditEntries` migration,
/// publishes the resulting seq to the daemon's high-water observer, and reloads
/// `entries_vec` / `effective_entries` so the caller sees the repaired log.
/// No-op when the effective log already has a header.
///
/// Returns whether a header was written, which is how the worker recognizes the
/// activation that brought this session into existence.
async fn maybe_insert_remote_header(args: MaybeInsertRemoteHeaderArgs<'_>) -> Result<bool> {
    let MaybeInsertRemoteHeaderArgs {
        backend,
        config,
        input,
        session_id,
        working_dir,
        header_insert_observer,
        entries_vec,
        effective_entries,
    } = args;

    if !should_insert_remote_header(effective_entries) {
        return Ok(false);
    }
    let Some((first_user_seq, last_user_seq, replacements)) =
        build_remote_header_insert_replacements(
            entries_vec,
            effective_entries,
            config,
            input,
            session_id,
            working_dir,
        )?
    else {
        return Ok(false);
    };

    let edit_entry = SessionLogEntry::EditEntries {
        from: first_user_seq,
        to: last_user_seq,
        replacements,
    };
    let message_id = remote_header_insert_message_id(session_id, first_user_seq);
    let insert_seq =
        crate::nats_session_log::NatsSessionLog::new(backend.jetstream(), session_id.to_string())
            .append_event_with_message_id_async(&edit_entry, message_id)
            .await?;
    debug!("inserted header via EditEntries js{insert_seq}");
    // Publish the migration seq so the daemon advances its activation high-water
    // cursor past the re-mapped leading-user block. The migration re-maps those
    // users onto `insert_seq`; the turn that runs this activation answers them,
    // so without this the drain would re-fold them (seq > pre-migration cursor)
    // and re-run the turn (S3).
    if let Some(observer) = header_insert_observer {
        observer.fetch_max(insert_seq, std::sync::atomic::Ordering::Relaxed);
    }
    *entries_vec = backend.load_events_blocking()?;
    *effective_entries = harnx_core::session_reconstruct::apply_log_mutations_nats(entries_vec)?;
    Ok(true)
}

/// Write a new session header and load the session.
pub(crate) async fn write_header_and_load_session(
    backend: &NatsSessionLogBackend,
    config: &GlobalConfig,
    input: &Input,
    session_index: Option<&async_nats::jetstream::kv::Store>,
    session_id: &str,
    working_dir: Option<&std::path::Path>,
) -> Result<harnx_core::session::Session> {
    let header = build_remote_session_header(config, input, session_id, working_dir)?;
    backend.append_event_blocking(&header)?;
    if let Some(store) = session_index {
        if let Err(err) = upsert_session_index_record(store, &header, None).await {
            log::warn!(
                "failed to upsert remote session index after header write: session_id={} err={err:#}",
                session_id
            );
        }
    }
    let entries = backend.load_events_blocking()?;
    crate::nats_session_log::load_session_from_entries(&entries, session_id)
}

#[cfg(test)]
mod tests {
    use super::{
        agent_resolved_hooks, dispatch_session_start, fold_new_user_messages_since, SessionOrigin,
        SessionStartDispatch,
    };
    use crate::config::Config;
    use crate::nats_hook_provider::{DiscoveredHook, NatsHookProvider};
    use chrono::{TimeZone, Utc};
    use harnx_core::hooks::{HookEvent, HookOutcome, HookPayload, HookResult, HookResultControl};
    use harnx_core::instance::ServerScope;
    use harnx_core::message::{MessageContent, MessageRole};
    use harnx_core::session::SessionLogEntry;
    use harnx_hookset::{FailPolicy, HookSpec};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// A provider whose only route is a SessionStart hook recording every
    /// payload it receives.
    fn recording_session_start_provider() -> (NatsHookProvider, Arc<Mutex<Vec<HookPayload>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let provider = NatsHookProvider::from_request_handler(
            ServerScope::from_string("session-start-test"),
            vec![DiscoveredHook {
                server: "lifecycle".to_string(),
                display_label: None,
                spec: HookSpec {
                    event: "SessionStart".to_string(),
                    matcher: None,
                    priority: 0,
                    timeout_secs: Some(1),
                    fail_policy: FailPolicy::Closed,
                },
            }],
            Arc::new(move |_subject, payload: HookPayload| {
                recorder.lock().expect("recorder lock").push(payload);
                HookOutcome {
                    control: HookResultControl::Continue,
                    result: HookResult::default(),
                }
            }),
        );
        (provider, seen)
    }

    #[tokio::test]
    async fn created_session_dispatches_session_start_to_worker_hooks() {
        let (provider, seen) = recording_session_start_provider();

        dispatch_session_start(SessionStartDispatch {
            origin: SessionOrigin::Created,
            provider: Some(&provider),
            session_id: "fresh-session",
            cwd: PathBuf::from("/tmp/project"),
            model: "test:test-model".to_string(),
            pending_async_context: None,
        })
        .await;

        let seen = seen.lock().expect("recorder lock");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].session_id, "fresh-session");
        assert_eq!(seen[0].cwd, PathBuf::from("/tmp/project"));
        let HookEvent::SessionStart { source, model } = &seen[0].hook_event else {
            panic!("expected SessionStart, got {:?}", seen[0].hook_event);
        };
        assert_eq!(source, "startup");
        assert_eq!(model, "test:test-model");
    }

    #[tokio::test]
    async fn resumed_session_does_not_redispatch_session_start() {
        let (provider, seen) = recording_session_start_provider();

        dispatch_session_start(SessionStartDispatch {
            origin: SessionOrigin::Resumed,
            provider: Some(&provider),
            session_id: "existing-session",
            cwd: PathBuf::from("/tmp/project"),
            model: "test:test-model".to_string(),
            pending_async_context: None,
        })
        .await;

        assert!(seen.lock().expect("recorder lock").is_empty());
    }

    fn user_entry(id: &str, text: &str, timestamp: chrono::DateTime<Utc>) -> SessionLogEntry {
        SessionLogEntry::Message {
            id: Some(id.to_string()),
            role: MessageRole::User,
            content: MessageContent::Text(text.to_string()),
            timestamp: Some(timestamp),
            fence_token: None,
        }
    }

    #[test]
    fn session_hook_resolution_excludes_instance_hooks() {
        let config = Config {
            data: harnx_core::config_data::ConfigData {
                hooks: Some(harnx_core::hooks::HooksConfig {
                    max_resume: None,
                    entries: vec![harnx_core::hooks::HookConfig {
                        command: "harnx-claude-compatible-hook-server --event SessionStart --timeout 30 -- echo global".to_string(),
                        status_message: None,
                        async_hook: None,
                        package_dir: None,
                    }],
                }),
                ..harnx_core::config_data::ConfigData::default()
            },
            ..Config::default()
        };
        let config = std::sync::Arc::new(parking_lot::RwLock::new(config));

        assert!(agent_resolved_hooks(&config).entries.is_empty());
    }

    #[test]
    fn session_hook_resolution_keeps_agent_override_of_global_hook() {
        let global_hook = harnx_core::hooks::HookConfig {
            command: "harnx-claude-compatible-hook-server --event SessionStart --timeout 30 -- echo global".to_string(),
            status_message: None,
            async_hook: None,
            package_dir: None,
        };
        let agent_config = harnx_core::agent_config::AgentConfig::from_markdown(
            "override-agent",
            "---\nhooks:\n  entries:\n    - command: harnx-claude-compatible-hook-server --event SessionStart --timeout 30 -- echo agent\n---\nprompt",
        )
        .expect("agent config");
        let config = Config {
            data: harnx_core::config_data::ConfigData {
                hooks: Some(harnx_core::hooks::HooksConfig {
                    max_resume: None,
                    entries: vec![global_hook],
                }),
                ..harnx_core::config_data::ConfigData::default()
            },
            agent: Some(crate::config::Agent::new(agent_config)),
            ..Config::default()
        };
        let config = std::sync::Arc::new(parking_lot::RwLock::new(config));

        let hooks = agent_resolved_hooks(&config);
        assert_eq!(hooks.entries.len(), 1);
        assert_eq!(
            hooks.entries[0].command,
            "harnx-claude-compatible-hook-server --event SessionStart --timeout 30 -- echo agent"
        );
    }

    #[test]
    fn fold_new_user_messages_since_excludes_retracted_messages() {
        let entries = vec![
            (
                1,
                user_entry(
                    "msg-1",
                    "retracted message",
                    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 1).unwrap(),
                ),
            ),
            (
                2,
                SessionLogEntry::EditEntries {
                    from: 1,
                    to: 1,
                    replacements: vec![],
                },
            ),
            (
                3,
                user_entry(
                    "msg-3",
                    "valid message",
                    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 3).unwrap(),
                ),
            ),
        ];

        let (messages, latest_seq) = fold_new_user_messages_since(&entries, None);

        assert_eq!(messages.len(), 1, "retracted message must be excluded");
        assert_eq!(messages[0].content.to_text(), "valid message");
        assert_eq!(messages[0].log_seq, Some(3));
        assert_eq!(
            messages[0].log_timestamp,
            Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 3).unwrap())
        );
        assert_eq!(latest_seq, Some(3));
    }

    #[test]
    fn fold_new_user_messages_since_skips_non_user_entries_but_tracks_latest_user_seq() {
        let entries = vec![
            (
                1,
                user_entry(
                    "msg-1",
                    "first valid",
                    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 1).unwrap(),
                ),
            ),
            (
                2,
                SessionLogEntry::Message {
                    id: Some("assistant-2".to_string()),
                    role: MessageRole::Assistant,
                    content: MessageContent::Text("assistant reply".to_string()),
                    timestamp: Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 2).unwrap()),
                    fence_token: None,
                },
            ),
            (
                3,
                user_entry(
                    "msg-3",
                    "second valid",
                    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 3).unwrap(),
                ),
            ),
        ];

        let (messages, latest_seq) = fold_new_user_messages_since(&entries, None);

        let folded: Vec<_> = messages
            .iter()
            .map(|message| {
                (
                    message.content.to_text(),
                    message.log_seq,
                    message.log_timestamp,
                )
            })
            .collect();
        assert_eq!(
            folded,
            vec![
                (
                    "first valid".to_string(),
                    Some(1),
                    Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 1).unwrap())
                ),
                (
                    "second valid".to_string(),
                    Some(3),
                    Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 3).unwrap())
                ),
            ]
        );
        assert_eq!(
            latest_seq,
            Some(3),
            "latest_seq must be max consumed user-message seq"
        );
    }

    #[test]
    fn fold_new_user_messages_since_cursor_semantics_with_retracts() {
        let entries = vec![
            (
                1,
                user_entry(
                    "msg-1",
                    "retracted",
                    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 1).unwrap(),
                ),
            ),
            (
                2,
                SessionLogEntry::EditEntries {
                    from: 1,
                    to: 1,
                    replacements: vec![],
                },
            ),
            (
                3,
                user_entry(
                    "msg-3",
                    "first valid",
                    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 3).unwrap(),
                ),
            ),
            (
                4,
                user_entry(
                    "msg-4",
                    "second valid",
                    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 4).unwrap(),
                ),
            ),
        ];

        let (messages, latest_seq) = fold_new_user_messages_since(&entries, Some(3));

        assert_eq!(
            messages.len(),
            1,
            "entries with seq <= cursor must be skipped after mutations"
        );
        assert_eq!(messages[0].content.to_text(), "second valid");
        assert_eq!(
            messages[0].log_seq,
            Some(4),
            "returned message must preserve original seq for stamping"
        );
        assert_eq!(
            messages[0].log_timestamp,
            Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 4).unwrap())
        );
        assert_eq!(
            latest_seq,
            Some(4),
            "latest_seq must track max consumed user-message seq"
        );
    }
}

//! Agent loop entrypoint for NATS-backed sessions.

use super::backend::{FencedSessionLogSink, NatsSessionLogBackend};
use super::hook_supervisor::{HookServerStartConfig, HookServerSupervisor};
use crate::agent_loop::OnToolRoundFn;
use crate::config::{resolve_local_nats_server_config, GlobalConfig, Input};
use crate::nats_attachments::SessionAttachmentSync;
use crate::nats_event_sink::NatsEventSink;
use crate::nats_hook_provider::{
    dispatch_hook_event, HookDispatchMeta, HookEventDispatch, NatsHookProvider,
};
use crate::nats_lease::NatsSessionLease;
use crate::nats_metrics;
use crate::nats_session::{NatsSession, NatsSessionConfig};
use crate::nats_session_metadata::SessionMetadataStore;
use crate::tool_context::{discover_nats_hook_provider_fresh, discover_nats_tool_provider_fresh};
use crate::utils::AbortSignal;
use anyhow::{Context, Result};
use async_nats::jetstream;
use harnx_core::message::Message;
use harnx_core::session::SessionLogEntry;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

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
    pub token_budget: Option<u64>,
    pub call_fn: Option<crate::agent_loop::AgentCallFn>,
    pub lease: Option<Arc<NatsSessionLease>>,
    /// Route used by this worker. Same-cluster handoffs reuse it so local
    /// frontend-affine workers stay targeted while persistent workers remain
    /// cluster-shared.
    pub activation_route: super::SessionActivationRoute,
    /// Source-session sink used for ordered handoff control-event delivery.
    pub event_sink: Option<Arc<NatsEventSink>>,
    pub after_seq_observer: Option<Arc<AtomicU64>>,
    pub session_metadata: Option<&'a SessionMetadataStore>,
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingHitlApproval {
    pub seq: u64,
    pub tool_round_seq: u64,
    pub tool_call_id: String,
    pub summary: String,
}

pub(crate) fn derive_pending_hitl_approvals(
    entries: &[(u64, SessionLogEntry)],
) -> Result<Vec<PendingHitlApproval>> {
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(entries)?;
    let Some(orphan) = find_orphan_tool_calls(&effective).into_iter().last() else {
        return Ok(Vec::new());
    };
    let round_entries = tool_round_entries(&effective, orphan.seq);
    Ok(round_entries
        .iter()
        .filter_map(|(request_seq, entry)| match entry {
            SessionLogEntry::HitlApprovalRequested {
                tool_call_id,
                summary,
                ..
            } if orphan
                .calls
                .iter()
                .any(|call| call.id.as_deref() == Some(tool_call_id.as_str()))
                && !round_entries.iter().any(|(decision_seq, decision)| {
                    decision_seq > request_seq
                        && matches!(
                            decision,
                            SessionLogEntry::HitlApprovalDecision {
                                tool_call_id: decided_id,
                                ..
                            } if decided_id == tool_call_id
                        )
                }) =>
            {
                Some(PendingHitlApproval {
                    seq: *request_seq,
                    tool_round_seq: orphan.seq,
                    tool_call_id: tool_call_id.clone(),
                    summary: summary.clone(),
                })
            }
            _ => None,
        })
        .collect())
}

fn tool_round_entries(
    entries: &[(u64, SessionLogEntry)],
    tool_calls_seq: u64,
) -> &[(u64, SessionLogEntry)] {
    let start = entries.partition_point(|(seq, _)| *seq <= tool_calls_seq);
    let end = entries[start..]
        .iter()
        .position(|(_, entry)| {
            matches!(
                entry,
                SessionLogEntry::ToolCalls { .. } | SessionLogEntry::ToolResults { .. }
            )
        })
        .map_or(entries.len(), |offset| start + offset);
    &entries[start..end]
}

fn hitl_managed_tool_call_ids(
    entries: &[(u64, SessionLogEntry)],
    tool_calls_seq: u64,
) -> std::collections::HashSet<&str> {
    tool_round_entries(entries, tool_calls_seq)
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::HitlApprovalRequested { tool_call_id, .. } => {
                Some(tool_call_id.as_str())
            }
            _ => None,
        })
        .collect()
}

#[derive(Clone, Debug)]
struct HitlToolRoundContinuation {
    output: String,
    thought: Option<String>,
    tool_calls: Vec<harnx_core::tool::ToolCall>,
    decisions: Vec<crate::agent_loop::ToolApprovalDecision>,
}

fn derive_hitl_tool_round_continuation(
    entries: &[(u64, SessionLogEntry)],
) -> Result<Option<HitlToolRoundContinuation>> {
    use std::collections::HashMap;

    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(entries)?;
    let Some(orphan) = find_orphan_tool_calls(&effective).into_iter().last() else {
        return Ok(None);
    };
    let round_entries = tool_round_entries(&effective, orphan.seq);
    let request_ids = hitl_managed_tool_call_ids(&effective, orphan.seq);
    if !orphan.calls.iter().any(|call| {
        call.id
            .as_deref()
            .is_some_and(|id| request_ids.contains(id))
    }) {
        return Ok(None);
    }
    let mut seen_requests = std::collections::HashSet::new();
    let mut decisions: HashMap<&str, (bool, Option<String>)> = HashMap::new();
    for (_, entry) in round_entries {
        match entry {
            SessionLogEntry::HitlApprovalRequested { tool_call_id, .. } => {
                seen_requests.insert(tool_call_id.as_str());
            }
            SessionLogEntry::HitlApprovalDecision {
                tool_call_id,
                approved,
                note,
                ..
            } if seen_requests.contains(tool_call_id.as_str()) => {
                decisions.insert(tool_call_id.as_str(), (*approved, note.clone()));
            }
            _ => {}
        }
    }
    let decisions = orphan
        .calls
        .iter()
        .filter_map(|call| {
            let id = call.id.as_deref()?;
            let (approved, note) = decisions.get(id)?.clone();
            Some(crate::agent_loop::ToolApprovalDecision {
                tool_call_id: id.to_string(),
                approved,
                reason: note,
            })
        })
        .collect();
    Ok(Some(HitlToolRoundContinuation {
        output: orphan.text,
        thought: orphan.thought,
        tool_calls: orphan.calls,
        decisions,
    }))
}
fn build_hitl_approval_request_callback(
    jetstream: &jetstream::Context,
    session_id: &str,
    lease: &Arc<NatsSessionLease>,
    event_sink: Option<&Arc<NatsEventSink>>,
    after_seq_observer: Option<&Arc<AtomicU64>>,
) -> crate::agent_loop::OnHitlApprovalRequiredFn {
    let backend = NatsSessionLogBackend::new(jetstream.clone(), session_id)
        .with_after_seq_observer(
            after_seq_observer
                .cloned()
                .unwrap_or_else(|| Arc::new(AtomicU64::new(0))),
        );
    let sink = FencedSessionLogSink::new(backend.clone(), Arc::clone(lease));
    let lease = Arc::clone(lease);
    let event_sink = event_sink.cloned();
    Arc::new(move |deferred| {
        anyhow::ensure!(
            lease.is_held(),
            "session lease lost before HITL approval request"
        );
        let tool_call_id = deferred
            .call
            .id
            .clone()
            .context("deferred tool call has no tool_call_id")?;
        let summary = deferred
            .reason
            .clone()
            .filter(|reason| !reason.trim().is_empty())
            .unwrap_or_else(|| format!("Approve tool call `{}`", deferred.call.name));
        let entry = SessionLogEntry::HitlApprovalRequested {
            tool_call_id: tool_call_id.clone(),
            summary,
            fence_token: lease.fence_token(),
        };
        for _ in 0..3 {
            anyhow::ensure!(
                lease.is_held(),
                "session lease lost before HITL approval request"
            );
            let entries = backend.load_events_blocking()?;
            let pending = derive_pending_hitl_approvals(&entries)?;
            if let Some(oldest) = pending.first() {
                log::warn!(
                    "session already has pending HITL approval; retaining oldest request: tool_call_id={} seq={}",
                    oldest.tool_call_id,
                    oldest.seq
                );
                return Ok(oldest.tool_call_id.clone());
            }
            let expected_last_sequence = entries.last().map_or(0, |(seq, _)| *seq);
            if sink
                .append_hitl_event_cas_blocking(&entry, expected_last_sequence)?
                .is_some()
            {
                if let Some(event_sink) = &event_sink {
                    event_sink.publish_session_updated();
                }
                return Ok(tool_call_id);
            }
        }
        anyhow::bail!("HITL approval request lost repeated concurrent append races")
    })
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
                    return Ok(());
                }
            };
            let current = match cursor.load(std::sync::atomic::Ordering::SeqCst) {
                0 => None,
                seq => Some(seq),
            };
            let (messages, latest_seq) = fold_new_user_messages_since(&tail, current);
            if messages.is_empty() {
                return Ok(());
            }
            merged_input.set_injected_user_text(fold_user_messages(&messages));
            if let Some(seq) = latest_seq {
                cursor.store(seq, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(())
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NatsAgentLoopOutcome {
    Completed,
    AwaitingHitlApproval,
    HandoffDispatched,
}

/// Like [`run_agent_loop_with_nats`], but fence-guarded by the holding lease
/// (P2.2). When `lease` is `Some`, every worker-originated append is gated on
/// `lease.is_held()` and stamped with the lease fence, and the session resume
/// is aborted if the persisted log tail already carries a fence GREATER than
/// the lease revision this worker holds (a newer worker has taken over).
pub async fn run_agent_loop_with_nats_inner(args: RunAgentLoopArgs<'_>) -> Result<()> {
    run_agent_loop_with_nats_outcome(args).await.map(drop)
}

pub(crate) async fn run_agent_loop_with_nats_outcome(
    args: RunAgentLoopArgs<'_>,
) -> Result<NatsAgentLoopOutcome> {
    let RunAgentLoopArgs {
        cluster_key,
        manage_servers,
        session_id,
        config,
        instance_id,
        initial_input,
        abort_signal,
        token_budget,
        call_fn,
        lease,
        activation_route,
        event_sink,
        after_seq_observer,
        session_metadata,
        on_tool_round,
        working_dir,
    } = args;
    let (jetstream_ctx, session_origin, attachment_sync) =
        prepare_agent_session(PrepareAgentSessionParams {
            cluster_key,
            session_id,
            config: &config,
            instance_id: &instance_id,
            abort_signal: &abort_signal,
            lease: lease.as_ref(),
            after_seq_observer: after_seq_observer.clone(),
            session_metadata,
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

    let on_hitl_approval_required = lease.as_ref().map(|lease| {
        build_hitl_approval_request_callback(
            &jetstream_ctx,
            session_id,
            lease,
            event_sink.as_ref(),
            after_seq_observer.as_ref(),
        )
    });
    let ctx = build_agent_loop_context(AgentContextParams {
        config: config.clone(),
        instance_id,
        abort_signal: abort_signal.clone(),
        token_budget,
        call_fn,
        on_tool_round,
        on_hitl_approval_required,
        working_dir,
    })
    .await;

    // After hook reconciliation and provider discovery so the dispatch reaches
    // both global and agent hook servers.
    dispatch_context_session_start(&ctx, session_origin, session_id).await;

    let backend = NatsSessionLogBackend::new(jetstream_ctx.clone(), session_id);
    let hitl_continuation = derive_hitl_tool_round_continuation(&backend.load_events_blocking()?)?;
    let segment_args = AgentLoopSegmentArgs {
        source_session_id: session_id,
        cluster_key,
        config: config.clone(),
        ctx,
        input: initial_input,
        abort_signal,
        jetstream_ctx: jetstream_ctx.clone(),
        activation_route,
        event_sink,
        lease,
    };
    let result = if let Some(continuation) = hitl_continuation {
        run_hitl_continuation_segment(segment_args, continuation).await
    } else {
        run_agent_loop_segment(segment_args).await
    };
    finish_agent_loop(hook_supervisor, attachment_sync, result).await
}

async fn finish_agent_loop(
    hook_supervisor: Option<HookServerSupervisor>,
    attachment_sync: SessionAttachmentSync,
    result: Result<NatsAgentLoopOutcome>,
) -> Result<NatsAgentLoopOutcome> {
    shutdown_agent_hooks(hook_supervisor).await;
    attachment_sync.finish(result).await
}

/// Shut down activation-scoped hooks on both successful and failed turns.
async fn shutdown_agent_hooks(mut supervisor: Option<HookServerSupervisor>) {
    if let Some(supervisor) = supervisor.as_mut() {
        supervisor.shutdown().await;
    }
}

struct PrepareAgentSessionParams<'a> {
    cluster_key: &'a str,
    session_id: &'a str,
    config: &'a GlobalConfig,
    instance_id: &'a harnx_core::instance::ServerScope,
    abort_signal: &'a AbortSignal,
    lease: Option<&'a Arc<NatsSessionLease>>,
    after_seq_observer: Option<Arc<AtomicU64>>,
    session_metadata: Option<&'a SessionMetadataStore>,
}

async fn prepare_agent_session(
    params: PrepareAgentSessionParams<'_>,
) -> Result<(jetstream::Context, SessionOrigin, SessionAttachmentSync)> {
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
        lease: params.lease.map(Arc::as_ref),
        session_metadata: params.session_metadata,
        session_id: params.session_id,
        abort_signal: params.abort_signal,
    })
    .await?;
    attach_session_to_config(AttachSessionParams {
        config: params.config,
        session,
        backend: &backend,
        lease: params.lease,
        metadata: params.session_metadata,
    });
    let attachment_sync = SessionAttachmentSync::prepare(
        jetstream.clone(),
        params.config.clone(),
        params.cluster_key,
        params.session_id,
    )
    .await?;
    Ok((jetstream, origin, attachment_sync))
}

struct AgentContextParams {
    config: GlobalConfig,
    instance_id: harnx_core::instance::ServerScope,
    abort_signal: AbortSignal,
    token_budget: Option<u64>,
    call_fn: Option<crate::agent_loop::AgentCallFn>,
    on_tool_round: Option<OnToolRoundFn>,
    on_hitl_approval_required: Option<crate::agent_loop::OnHitlApprovalRequiredFn>,
    working_dir: Option<std::path::PathBuf>,
}

async fn build_agent_loop_context(
    params: AgentContextParams,
) -> crate::agent_loop::AgentLoopContext {
    let config_snapshot = params.config.read().clone();
    let usage_at_start = config_snapshot
        .session
        .as_ref()
        .map(|session| session.completion_usage().clone())
        .unwrap_or_default();
    let active_package = config_snapshot.active_package();
    // Activation has already waited for this session's on-demand tool
    // servers to register. Replace any snapshot captured while startup was
    // still in flight before the first model request reuses the tool cache.
    discover_nats_tool_provider_fresh(
        &config_snapshot,
        &params.instance_id,
        active_package.as_deref(),
    )
    .await;
    let nats_hook_provider =
        discover_nats_hook_provider_fresh(&config_snapshot, &params.instance_id).await;
    crate::agent_loop::AgentLoopContext {
        config: params.config,
        instance_id: params.instance_id,
        abort_signal: params.abort_signal,
        token_budget: params.token_budget,
        usage_at_start,
        call_fn: params.call_fn,
        on_tool_round: params.on_tool_round,
        on_hitl_approval_required: params.on_hitl_approval_required,
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
    execution: Option<harnx_execution_control::OperationRef>,
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
    let execution = ctx
        .config
        .read()
        .execution_control
        .as_ref()
        .map(|(_, reference)| reference.clone());
    dispatch_session_start(SessionStartDispatch {
        execution,
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
            execution: params.execution,
            session_id: params.session_id.to_string(),
            cwd: params.cwd,
            resume_count: 0,
        },
        pending_async_context: params.pending_async_context,
    })
    .await;
}

struct AgentLoopSegmentArgs<'a> {
    source_session_id: &'a str,
    cluster_key: &'a str,
    config: GlobalConfig,
    ctx: crate::agent_loop::AgentLoopContext,
    input: Input,
    abort_signal: AbortSignal,
    jetstream_ctx: jetstream::Context,
    activation_route: super::SessionActivationRoute,
    event_sink: Option<Arc<NatsEventSink>>,
    lease: Option<Arc<NatsSessionLease>>,
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

async fn run_agent_loop_segment(args: AgentLoopSegmentArgs<'_>) -> Result<NatsAgentLoopOutcome> {
    let args_ref = &args;
    let result = crate::agent_loop::run_agent_loop_with_before_end(
        &args.ctx,
        args.input.clone(),
        |result| {
            let handoff = match result {
                crate::agent_loop::LoopResult::Completed
                | crate::agent_loop::LoopResult::AwaitingHitlApproval { .. } => None,
                crate::agent_loop::LoopResult::HandoffRequested {
                    agent,
                    session_id,
                    prompt,
                    tool_call_id,
                } => Some((
                    agent.clone(),
                    session_id.clone(),
                    prompt.clone(),
                    tool_call_id.clone(),
                )),
            };
            async move {
                if let Some((agent, session_id, prompt, tool_call_id)) = handoff {
                    dispatch_nats_handoff(args_ref, agent, session_id, prompt, tool_call_id)
                        .await?;
                }
                Ok(())
            }
        },
    )
    .await?;
    Ok(match result {
        crate::agent_loop::LoopResult::Completed => NatsAgentLoopOutcome::Completed,
        crate::agent_loop::LoopResult::AwaitingHitlApproval { .. } => {
            NatsAgentLoopOutcome::AwaitingHitlApproval
        }
        crate::agent_loop::LoopResult::HandoffRequested { .. } => {
            NatsAgentLoopOutcome::HandoffDispatched
        }
    })
}

async fn run_hitl_continuation_segment(
    args: AgentLoopSegmentArgs<'_>,
    continuation: HitlToolRoundContinuation,
) -> Result<NatsAgentLoopOutcome> {
    if let Some(lease) = &args.lease {
        anyhow::ensure!(
            lease.revalidate_ownership().await?,
            "session lease lost before approved HITL tool execution"
        );
    }
    let pending_interrupt_ids = continuation
        .tool_calls
        .iter()
        .filter_map(|call| call.id.clone())
        .collect();
    let result = crate::agent_loop::continue_agent_loop_from_tool_round(
        &args.ctx,
        args.input.clone(),
        continuation.output,
        continuation.thought,
        continuation.tool_calls,
        continuation.decisions,
        pending_interrupt_ids,
    )
    .await?;
    if let crate::agent_loop::LoopResult::HandoffRequested {
        agent,
        session_id,
        prompt,
        tool_call_id,
    } = &result
    {
        dispatch_nats_handoff(
            &args,
            agent.clone(),
            session_id.clone(),
            prompt.clone(),
            tool_call_id.clone(),
        )
        .await?;
    }
    Ok(match result {
        crate::agent_loop::LoopResult::Completed => NatsAgentLoopOutcome::Completed,
        crate::agent_loop::LoopResult::AwaitingHitlApproval { .. } => {
            NatsAgentLoopOutcome::AwaitingHitlApproval
        }
        crate::agent_loop::LoopResult::HandoffRequested { .. } => {
            NatsAgentLoopOutcome::HandoffDispatched
        }
    })
}

async fn dispatch_nats_handoff(
    args: &AgentLoopSegmentArgs<'_>,
    agent: String,
    session_id: Option<String>,
    prompt: String,
    handoff_tool_call_id: Option<String>,
) -> Result<()> {
    let requested_session_id = session_id.filter(|session_id| !session_id.trim().is_empty());
    let destination = resolve_handoff_destination(args, &agent).await?;

    let target_session = NatsSession::new(
        NatsSessionConfig {
            cluster: destination.cluster,
            initializer: crate::SessionInitializer::named(
                destination.agent,
                harnx_core::agent_config::AgentVariables::default(),
            ),
            session_id: requested_session_id,
            activation_route: destination.activation_route,
        },
        destination.client,
        destination.jetstream,
        args.abort_signal.clone(),
    )
    .await
    .with_context(|| format!("create handoff target session for '{agent}'"))?;
    let enqueued = target_session
        .enqueue(&prompt)
        .await
        .with_context(|| format!("queue handoff prompt for '{agent}'"))?;
    log::debug!(
        "handoff queued: source_session_id={} target_agent={} target_session_id={} user_seq={}",
        args.source_session_id,
        agent,
        enqueued.session_id,
        enqueued.user_msg_seq
    );

    emit_handoff_committed(
        args,
        &agent,
        destination.committed_agent,
        enqueued.session_id,
        handoff_tool_call_id,
    )
    .await
}

struct HandoffDestination {
    agent: String,
    cluster: String,
    activation_route: super::SessionActivationRoute,
    committed_agent: String,
    client: async_nats::Client,
    jetstream: jetstream::Context,
}

async fn resolve_handoff_destination(
    args: &AgentLoopSegmentArgs<'_>,
    agent: &str,
) -> Result<HandoffDestination> {
    use harnx_core::agent_ref::AgentRef;

    match AgentRef::parse(agent) {
        AgentRef::Local(target_agent) => {
            let target_agent = target_agent.into_owned();
            let committed_agent = match args.activation_route {
                super::SessionActivationRoute::ClusterShared => {
                    format!("{target_agent}@{}", args.cluster_key)
                }
                super::SessionActivationRoute::WorkerTargeted { .. } => target_agent.clone(),
            };
            Ok(HandoffDestination {
                agent: target_agent,
                cluster: args.cluster_key.to_string(),
                activation_route: args.activation_route.clone(),
                committed_agent,
                client: args.jetstream_ctx.client().clone(),
                jetstream: args.jetstream_ctx.clone(),
            })
        }
        AgentRef::Remote {
            agent: target_agent,
            cluster,
        } => {
            let target_cluster = cluster.into_owned();
            let config = args.config.read().clone();
            let client = config
                .nats_client(&target_cluster)
                .await
                .with_context(|| format!("connect to handoff target cluster '{target_cluster}'"))?;
            Ok(HandoffDestination {
                agent: target_agent.into_owned(),
                cluster: target_cluster,
                activation_route: super::SessionActivationRoute::ClusterShared,
                committed_agent: agent.to_string(),
                jetstream: jetstream::new(client.clone()),
                client,
            })
        }
    }
}

async fn emit_handoff_committed(
    args: &AgentLoopSegmentArgs<'_>,
    requested_agent: &str,
    committed_agent: String,
    session_id: String,
    handoff_tool_call_id: Option<String>,
) -> Result<()> {
    use harnx_core::event::{AgentEvent, SessionEvent};

    let event_sink = match &args.event_sink {
        Some(event_sink) => Arc::clone(event_sink),
        None => Arc::new(
            NatsEventSink::new(
                args.jetstream_ctx.client().clone(),
                args.jetstream_ctx.clone(),
                args.source_session_id,
            )
            .await,
        ),
    };
    // Append durable HandoffCommitted entry to the source session's log
    // so that clients attaching after the handoff can navigate to the target.
    let backend = crate::nats_worker::backend::NatsSessionLogBackend::new(
        args.jetstream_ctx.clone(),
        args.source_session_id,
    );
    let after_seq = backend
        .append_event(&harnx_core::session::SessionLogEntry::HandoffCommitted {
            target_agent: committed_agent.clone(),
            target_session_id: session_id.clone(),
            handoff_tool_call_id: handoff_tool_call_id.clone(),
        })
        .await
        .with_context(|| {
            format!(
                "handoff target '{requested_agent}' session '{session_id}' was activated, but durable commit failed; the handoff will not survive reconnect"
            )
        })?;

    // Wake attached clients after durable control append
    event_sink.publish_session_updated();

    // Also emit advisory for real-time clients
    event_sink.emit_required(AgentEvent::Session(SessionEvent::HandoffCommitted {
        agent: committed_agent,
        session_id: session_id.clone(),
        handoff_tool_call_id: handoff_tool_call_id.clone(),
        after_seq: Some(after_seq),
    }));
    event_sink.flush().await.with_context(|| {
        format!(
            "handoff target '{requested_agent}' session '{session_id}' was activated, but confirmation delivery failed; open that session manually"
        )
    })
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
    lease: Option<&'a NatsSessionLease>,
    session_metadata: Option<&'a SessionMetadataStore>,
    session_id: &'a str,
    abort_signal: &'a AbortSignal,
}

/// Load a conversation-only transcript into state initialized from canonical
/// metadata, repairing orphan tool calls when resuming an interrupted turn.
async fn load_or_repair_session(
    params: LoadOrRepairSessionParams<'_>,
) -> Result<(harnx_core::session::Session, SessionOrigin)> {
    let LoadOrRepairSessionParams {
        backend,
        config,
        instance_id,
        lease,
        session_metadata,
        session_id,
        abort_signal,
    } = params;
    let store = session_metadata.context("NATS worker requires canonical session metadata")?;
    let metadata = store
        .get(session_id)
        .await?
        .with_context(|| format!("session '{session_id}' has no canonical metadata"))?
        .metadata;
    let previous_activity = store.get_activity(session_id).await?;
    let origin = if previous_activity
        .as_ref()
        .and_then(|activity| activity.first_activation_at)
        .is_none()
    {
        SessionOrigin::Created
    } else {
        SessionOrigin::Resumed
    };

    let mut entries_vec = backend.load_events_blocking()?;
    let effective_entries =
        harnx_core::session_reconstruct::apply_log_mutations_nats(&entries_vec)?;
    let preserve_hitl_pending = find_orphan_tool_calls(&effective_entries)
        .iter()
        .any(|orphan| {
            let hitl_ids = hitl_managed_tool_call_ids(&effective_entries, orphan.seq);
            orphan
                .calls
                .iter()
                .any(|call| call.id.as_deref().is_some_and(|id| hitl_ids.contains(id)))
        });
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
    let mut session = crate::config::session::new(&config.read(), session_id, None)?;
    session.id = session_id.to_string();
    session.session_id = Some(session_id.to_string());
    session.working_dir = None;
    session.git_branch = None;
    session.git_remote = None;
    session.terminal_session_id = None;
    session.agent_variables = metadata.variables.clone();
    session.title = metadata.title.value.clone();
    session.title_last_updated_tokens = if metadata.title.manual {
        usize::MAX
    } else {
        metadata.title.last_updated_tokens
    };
    let session = if preserve_hitl_pending {
        crate::nats_session_log::load_session_from_entries_with_metadata_preserving_pending(
            &entries_vec,
            session_id,
            session,
        )?
    } else {
        crate::nats_session_log::load_session_from_entries_with_metadata(
            &entries_vec,
            session_id,
            session,
        )?
    };
    store.mark_activated(session_id).await?;
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
    let orphan_calls: Vec<_> = find_orphan_tool_calls(effective_entries)
        .into_iter()
        .filter(|orphan| {
            let hitl_ids = hitl_managed_tool_call_ids(effective_entries, orphan.seq);
            !orphan
                .calls
                .iter()
                .any(|call| call.id.as_deref().is_some_and(|id| hitl_ids.contains(id)))
        })
        .collect();
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

/// Attach the reconstructed session to the shared config with the NATS append
/// sink for the unified persistence path. With a lease, use the fence-guarded
/// sink so writes from a fenced-out worker are rejected.
struct AttachSessionParams<'a> {
    config: &'a GlobalConfig,
    session: harnx_core::session::Session,
    backend: &'a NatsSessionLogBackend,
    lease: Option<&'a Arc<NatsSessionLease>>,
    metadata: Option<&'a SessionMetadataStore>,
}

fn attach_session_to_config(params: AttachSessionParams<'_>) {
    let AttachSessionParams {
        config,
        mut session,
        backend,
        lease,
        metadata,
    } = params;
    let sink: Arc<dyn crate::config::session::SessionAppendSink> = match lease {
        Some(lease) => Arc::new(
            FencedSessionLogSink::new(backend.clone(), Arc::clone(lease))
                .with_metadata_store(metadata.cloned()),
        ),
        None => Arc::new(backend.clone().with_metadata_store(metadata.cloned())),
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

    // User messages may arrive while a tool is still running, so only a
    // matching ToolResults entry or a subsequent ToolCalls batch closes the
    // current candidate. In particular, a synthetic repair appended after a
    // queued user must make a later scan idempotent.
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
                if let Some(previous) = last_tool_calls.take() {
                    orphans.push(previous);
                }
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

#[cfg(test)]
mod tests {
    use super::{
        agent_resolved_hooks, derive_hitl_tool_round_continuation, derive_pending_hitl_approvals,
        dispatch_session_start, find_orphan_tool_calls, fold_new_user_messages_since,
        SessionOrigin, SessionStartDispatch,
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
            execution: None,
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
            execution: None,
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

    fn tool_calls_entry(call_id: &str) -> SessionLogEntry {
        SessionLogEntry::ToolCalls {
            text: "working".to_string(),
            thought: None,
            calls: vec![harnx_core::tool::ToolCall::new(
                "search".to_string(),
                serde_json::json!({}),
                Some(call_id.to_string()),
                None,
            )],
            timestamp: None,
            fence_token: Some(7),
        }
    }

    fn tool_results_entry(call_id: &str) -> SessionLogEntry {
        SessionLogEntry::ToolResults {
            results: vec![harnx_core::session::ToolOutput {
                id: Some(call_id.to_string()),
                name: "search".to_string(),
                output: serde_json::json!({"ok": true}),
                markdown: None,
                content: Vec::new(),
                switch_agent: None,
            }],
            timestamp: None,
        }
    }

    #[test]
    fn reused_tool_call_id_requires_fresh_approval_in_current_tool_round() {
        let entries = vec![
            (1, tool_calls_entry("reused-call")),
            (
                2,
                SessionLogEntry::HitlApprovalRequested {
                    tool_call_id: "reused-call".to_string(),
                    summary: "Approve historical call".to_string(),
                    fence_token: 7,
                },
            ),
            (
                3,
                SessionLogEntry::HitlApprovalDecision {
                    tool_call_id: "reused-call".to_string(),
                    approved: true,
                    note: None,
                    fence_token: 7,
                },
            ),
            (4, tool_results_entry("reused-call")),
            (5, tool_calls_entry("reused-call")),
            (
                6,
                SessionLogEntry::HitlApprovalRequested {
                    tool_call_id: "reused-call".to_string(),
                    summary: "Approve current call".to_string(),
                    fence_token: 8,
                },
            ),
        ];

        let pending = derive_pending_hitl_approvals(&entries).expect("derive pending approval");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].seq, 6);
        assert_eq!(pending[0].summary, "Approve current call");

        let continuation = derive_hitl_tool_round_continuation(&entries)
            .expect("derive HITL continuation")
            .expect("current orphan is HITL-managed");
        assert!(
            continuation.decisions.is_empty(),
            "historical approval must not authorize current tool round"
        );
    }

    #[test]
    fn orphan_repair_result_after_queued_user_is_idempotent() {
        let entries = vec![
            (1, tool_calls_entry("call-1")),
            (
                2,
                user_entry(
                    "queued",
                    "queued correction",
                    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 2).unwrap(),
                ),
            ),
            (3, tool_results_entry("call-1")),
        ];

        assert!(find_orphan_tool_calls(&entries).is_empty());
    }

    #[test]
    fn tool_call_without_results_remains_orphaned_after_queued_user() {
        let entries = vec![
            (1, tool_calls_entry("call-1")),
            (
                2,
                user_entry(
                    "queued",
                    "queued correction",
                    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 2).unwrap(),
                ),
            ),
        ];

        let orphans = find_orphan_tool_calls(&entries);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].seq, 1);
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

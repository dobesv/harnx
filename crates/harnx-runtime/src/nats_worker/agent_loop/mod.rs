//! Agent loop entrypoint for NATS-backed sessions.

mod handoff;
mod handoff_dispatch;
mod handoff_recovery;
pub(super) use handoff_dispatch::HandoffDispatchArgs;
pub(super) use handoff_recovery::{reconcile as reconcile_pending_handoff, HandoffRecovery};

mod deadline_recovery;
mod middleware_recovery;
mod tool_recovery;
use super::backend::{FencedSessionLogSink, NatsSessionLogBackend};
use crate::agent_loop::OnToolRoundFn;
use crate::config::{GlobalConfig, Input};
use crate::nats_attachments::SessionAttachmentSync;
use crate::nats_event_sink::NatsEventSink;
use crate::nats_lease::NatsSessionLease;
use crate::nats_metrics;
use crate::nats_session::PendingHitlApproval;
use crate::nats_session_metadata::{SessionMetadata, SessionMetadataStore};
use crate::nats_tool_provider::NatsInFlightCalls;
use crate::tool_context::discover_nats_tool_provider_fresh;
use crate::utils::AbortSignal;
use anyhow::{Context, Result};
use async_nats::jetstream;
pub(super) use deadline_recovery::recover_completed_before_deadline;
use harnx_core::cid_url::SessionRef;
use harnx_core::message::Message;
use harnx_core::session::SessionLogEntry;
pub(super) use middleware_recovery::parks_recovery;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tool_recovery::repair_single_orphan;

#[derive(Clone)]
pub struct RunAgentLoopArgs<'a> {
    pub cluster_key: &'a str,
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

/// Detect an unresolved `CompactRequest` in the session log.
///
/// An unresolved request is one without a subsequent `CompactResult`
/// for the same `compaction_id`. Returns the compaction_id if found.
///
/// Scans backward from the most recent entry. Returns the `compaction_id` of the first
/// `CompactRequest` that has no corresponding `CompactResult`. This fixes both the dead-code
/// issue in forward scan (entries chronologically can't have a result before its request)
/// and correctly handles multiple pending requests by prioritizing the most recent.
///
/// See also: frontend tail guard in `nats_session/compaction_request.rs` (`decide_compact_submit`).
pub(crate) fn derive_pending_compaction(entries: &[(u64, SessionLogEntry)]) -> Option<String> {
    let mut resolved_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for (_, entry) in entries.iter().rev() {
        match entry {
            SessionLogEntry::CompactResult { compaction_id, .. } => {
                resolved_ids.insert(compaction_id.as_str());
            }
            SessionLogEntry::CompactRequest { compaction_id, .. }
                if !resolved_ids.contains(compaction_id.as_str()) =>
            {
                return Some(compaction_id.clone());
            }
            SessionLogEntry::CompactRequest { .. } => {}
            _ => {}
        }
    }

    None
}

pub(crate) fn derive_pending_hitl_approvals(
    entries: &[(u64, SessionLogEntry)],
) -> Result<Vec<PendingHitlApproval>> {
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(entries)?;
    let orphan = find_orphan_tool_calls(&effective).into_iter().last();

    let mut pending: Vec<_> = crate::interactions::derive_pending_interactions(&effective)?
        .into_iter()
        .map(|i| map_interaction_approval(i, orphan.as_ref()))
        .collect();

    if let Some(ref o) = orphan {
        let round_entries = tool_round_entries(&effective, o.seq);
        pending.extend(derive_legacy_pending(o, round_entries));
    }

    if pending.is_empty() && crate::interaction_checkpoint::guard::log_is_parked(&effective) {
        return Ok(Default::default());
    }

    Ok(pending)
}

fn map_interaction_approval(
    i: crate::interactions::PendingInteraction,
    orphan: Option<&PendingToolCalls>,
) -> PendingHitlApproval {
    let (tool_name, arguments) = orphan
        .and_then(|o| {
            o.calls
                .iter()
                .find(|c| c.id.as_deref() == Some(i.tool_call_id.as_str()))
        })
        .map_or((i.producer().to_owned(), i.request.payload.clone()), |c| {
            (c.name.clone(), c.arguments.clone())
        });
    PendingHitlApproval {
        request_seq: i.request_seq,
        tool_round_seq: i.tool_round_seq,
        tool_call_id: i.tool_call_id,
        tool_name,
        arguments,
        summary: i.request.summary,
        interaction_id: Some(i.interaction_id),
    }
}

fn derive_legacy_pending(
    orphan: &PendingToolCalls,
    round_entries: &[(u64, SessionLogEntry)],
) -> Vec<PendingHitlApproval> {
    let mut legacy = Vec::new();
    for (request_seq, entry) in round_entries {
        if let SessionLogEntry::HitlApprovalRequested {
            tool_call_id,
            summary,
            ..
        } = entry
        {
            let decided = round_entries.iter().any(|(decision_seq, decision)| {
                decision_seq > request_seq
                    && matches!(
                        decision,
                        SessionLogEntry::HitlApprovalDecision {
                            tool_call_id: decided_id,
                            ..
                        } | SessionLogEntry::HitlApprovalRequested {
                            tool_call_id: decided_id,
                            ..
                        } if decided_id == tool_call_id
                    )
            });
            if !decided {
                if let Some(call) = orphan
                    .calls
                    .iter()
                    .find(|call| call.id.as_deref() == Some(tool_call_id.as_str()))
                {
                    legacy.push(PendingHitlApproval {
                        request_seq: *request_seq,
                        tool_round_seq: orphan.seq,
                        tool_call_id: tool_call_id.clone(),
                        tool_name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        summary: summary.clone(),
                        interaction_id: None,
                    });
                }
            }
        }
    }
    legacy
}

/// Derive the attention sequence from log entries.
///
/// Returns the max stream seq among attention-producing entries:
/// - `TurnEnd` with `through_seq > 0` (final message turn boundary)
/// - `HitlApprovalRequested` (HITL approval request)
///
/// Used by `reconcile_attention_from_log` to repair lost bumps.
pub fn derive_attention_seq(entries: &[(u64, SessionLogEntry)]) -> u64 {
    entries
        .iter()
        .filter_map(|(seq, entry)| match entry {
            // TurnEnd with through_seq > 0 marks a complete turn
            SessionLogEntry::TurnEnd { through_seq, .. } if *through_seq > 0 => Some(*seq),
            // HitlApprovalRequested always bumps attention
            SessionLogEntry::HitlApprovalRequested { .. } => Some(*seq),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

fn tool_round_entries(
    entries: &[(u64, SessionLogEntry)],
    tool_calls_seq: u64,
) -> &[(u64, SessionLogEntry)] {
    crate::interactions::pending::entries_for_round(entries, tool_calls_seq)
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

fn round_continuation(
    orphan: PendingToolCalls,
    decisions: Vec<crate::agent_loop::ToolApprovalDecision>,
    legacy_request_ids: std::collections::BTreeSet<String>,
) -> HitlToolRoundContinuation {
    HitlToolRoundContinuation {
        round: crate::agent_loop::PendingToolRound {
            seq: orphan.seq,
            output: orphan.text,
            thought: orphan.thought,
            calls: orphan.calls,
        },
        decisions,
        legacy_request_ids,
    }
}

#[derive(Clone, Debug)]
struct HitlToolRoundContinuation {
    round: crate::agent_loop::PendingToolRound,
    decisions: Vec<crate::agent_loop::ToolApprovalDecision>,
    legacy_request_ids: std::collections::BTreeSet<String>,
}

fn derive_hitl_tool_round_continuation(
    entries: &[(u64, SessionLogEntry)],
) -> Result<Option<HitlToolRoundContinuation>> {
    use std::collections::HashMap;

    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(entries)?;
    let live_resume = crate::interaction_checkpoint::live_resume_owned(&effective);
    if !live_resume && crate::interaction_checkpoint::guard::log_is_parked(&effective) {
        return Ok(Default::default());
    }
    let Some(orphan) = find_orphan_tool_calls(&effective).into_iter().last() else {
        return Ok(None);
    };
    let round_entries = tool_round_entries(&effective, orphan.seq);
    let request_ids = hitl_managed_tool_call_ids(&effective, orphan.seq);
    if !live_resume
        && !orphan.calls.iter().any(|call| {
            call.id
                .as_deref()
                .is_some_and(|id| request_ids.contains(id))
        })
    {
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
    let legacy_request_ids = request_ids.into_iter().map(str::to_owned).collect();
    Ok(Some(round_continuation(
        orphan,
        decisions,
        legacy_request_ids,
    )))
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

pub(crate) use selected_input::fold_new_user_messages_since;

// Some(None) is an executable row missing its required admission identity.
pub(super) fn executable_user_message_id(entry: &SessionLogEntry) -> Option<Option<&str>> {
    match entry {
        SessionLogEntry::Message {
            id, role, content, ..
        } if is_executable_user_message(id.as_deref(), role, content) => Some(id.as_deref()),
        _ => None,
    }
}

fn is_executable_user_message(
    id: Option<&str>,
    role: &harnx_core::message::MessageRole,
    content: &harnx_core::message::MessageContent,
) -> bool {
    // Replayed runtime notes, hook-added context and valid timing notices are
    // context, not prompts.
    role.is_user()
        && !harnx_core::timing_identity::is_timing_notice(&content.to_text())
        && !harnx_core::runtime_note::is_runtime_context_id(id)
}

/// Verify that the durable user rows the fold selected for injection are all
/// admitted to this execution. Verifying exactly the selected rows matters:
/// scanning every effective row past the cursor would re-check historical
/// compaction copies whose admissions belong to older invocations and fail
/// with a competing-execution error even though the fold already deduplicated
/// them out of the injection.
async fn verify_injected_prompt_admissions(
    store: &SessionMetadataStore,
    session_id: &str,
    messages: &[harnx_core::message::Message],
    invocation_id: &str,
) -> Result<()> {
    for message in messages {
        // Include provenance in the failure so future incidents can be
        // diagnosed from the log_seq alone (issue #2493 arrived with no
        // session log attached).
        let note_like = message
            .content
            .to_text()
            .starts_with(harnx_core::runtime_note::RUNTIME_NOTE_PREFIX);
        let id = message
            .id
            .as_deref()
            .with_context(|| {
                format!(
                    "injected user row has no admission identity (log_seq={:?}, runtime-note-prefixed text={note_like})",
                    message.log_seq
                )
            })?;
        let admission = store
            .prompt_admission(session_id, id)
            .await?
            .context("injected user row has no durable admission")?;
        anyhow::ensure!(
            admission.invocation_id.as_str() == invocation_id,
            "cannot inject a competing execution admission"
        );
    }
    Ok(())
}

/// Inject user messages that arrived during a tool round into the next
/// request, and tell `in_flight` how far the turn has read the session log,
/// so a wait dispatched later doesn't stop for a message already injected.
pub(crate) fn build_mid_turn_injection_callback(
    backend: NatsSessionLogBackend,
    cursor: Arc<AtomicU64>,
    invocation_id: String,
    in_flight: NatsInFlightCalls,
) -> OnToolRoundFn {
    Arc::new(move |merged_input, _results| {
        let backend = backend.clone();
        let cursor = Arc::clone(&cursor);
        let invocation_id = invocation_id.clone();
        let in_flight = in_flight.clone();
        Box::pin(async move {
            let tail = match backend.load_events_latest_async().await {
                Ok(entries) => entries,
                Err(err) => {
                    log::warn!("failed to reload session log for mid-turn injection: {err}");
                    return Err(err);
                }
            };
            // Every executable message through the end of this read is either
            // folded in below or a copy of one the turn already has, so none
            // of them is still waiting. Marking the read's end rather than the
            // cursor also clears a compaction copy, which the fold never selects.
            let read_through = tail.last().map_or(0, |(seq, _)| *seq);
            let current = match cursor.load(std::sync::atomic::Ordering::SeqCst) {
                0 => None,
                seq => Some(seq),
            };
            let (messages, latest_seq) = fold_new_user_messages_since(&tail, current);
            if messages.is_empty() {
                in_flight.note_messages_consumed(backend.session_id(), read_through);
                return Ok(());
            }
            let store = backend
                .metadata_store_opt()
                .context("injection requires admission metadata")?;
            // Verify exactly what the fold selected (deduplicated, past the
            // cursor) rather than rescanning the whole tail: compaction copies
            // of older admissions must not fail this turn once any new input
            // unlocks the callback.
            verify_injected_prompt_admissions(
                store,
                backend.session_id(),
                &messages,
                &invocation_id,
            )
            .await?;
            merged_input.set_injected_user_messages(messages);
            if let Some(seq) = latest_seq {
                cursor.store(seq, std::sync::atomic::Ordering::SeqCst);
            }
            in_flight.note_messages_consumed(backend.session_id(), read_through);
            Ok(())
        })
    })
}

struct RepairOrphanToolCallsArgs<'a> {
    prepare_handoff_fn: Option<Arc<crate::tool::PrepareHandoffFn>>,
    config: GlobalConfig,
    instance_id: &'a harnx_core::instance::ServerScope,
    fence_token: Option<u64>,
    worker_id: Option<String>,
    session_id: &'a str,
    abort_signal: &'a AbortSignal,
    lease: Option<&'a NatsSessionLease>,
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
    Parked,
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

async fn setup_agent_loop_context(
    params: AgentContextParams,
    session_origin: SessionOrigin,
    session_id: &str,
) -> Result<crate::agent_loop::AgentLoopContext> {
    let ctx = Box::pin(build_agent_loop_context(params)).await;
    dispatch_context_session_start(&ctx, session_origin, session_id).await?;
    Ok(ctx)
}

async fn run_nats_loop_segment(
    segment_args: AgentLoopSegmentArgs<'_>,
    jetstream_ctx: jetstream::Context,
    session_id: &str,
    replicas: usize,
) -> Result<NatsAgentLoopOutcome> {
    let backend = NatsSessionLogBackend::new(jetstream_ctx, session_id, replicas);
    if parks_recovery(&backend, &segment_args.config).await? {
        return Ok(NatsAgentLoopOutcome::Parked);
    }
    let recovered = Box::pin(middleware_recovery::reconcile(&segment_args, &backend)).await?;
    let hitl_continuation = match recovered {
        Some(continuation) => Some(continuation),
        None => derive_hitl_tool_round_continuation(&backend.load_events_blocking()?)?,
    };
    segment_args.run(hitl_continuation).await
}

async fn prepare_session_for_agent_loop(
    args: &RunAgentLoopArgs<'_>,
) -> Result<(
    jetstream::Context,
    SessionOrigin,
    SessionAttachmentSync,
    Arc<crate::tool::PrepareHandoffFn>,
)> {
    Box::pin(prepare_agent_session(PrepareAgentSessionParams {
        cluster_key: args.cluster_key,
        activation_route: &args.activation_route,
        session_id: args.session_id,
        config: &args.config,
        instance_id: &args.instance_id,
        abort_signal: &args.abort_signal,
        lease: args.lease.as_ref(),
        after_seq_observer: args.after_seq_observer.clone(),
        session_metadata: args.session_metadata,
        selected_user_messages: &args.initial_input.durable_user_messages,
        replay_unmanaged: args.initial_input.skip_user_log_append,
    }))
    .await
}

pub(crate) async fn run_agent_loop_with_nats_outcome(
    args: RunAgentLoopArgs<'_>,
) -> Result<NatsAgentLoopOutcome> {
    // Direct callers need the same preflight as daemon activations, before
    // hydration can synthesize an orphan result or run a session-start hook.
    let snapshot = args.config.read().clone();
    let js = snapshot.nats_jetstream(args.cluster_key).await?;
    let replicas = snapshot
        .resolve_nats_server(args.cluster_key)
        .await?
        .resolved_replicas();
    let backend = NatsSessionLogBackend::new(js.clone(), args.session_id, replicas);
    if parks_recovery(&backend, &args.config).await? {
        return Ok(NatsAgentLoopOutcome::Parked);
    }
    let handoff = HandoffDispatchArgs {
        config: &args.config,
        source_session_id: args.session_id,
        cluster_key: args.cluster_key,
        jetstream_ctx: &js,
        activation_route: &args.activation_route,
        replicas,
        abort_signal: &args.abort_signal,
        lease: args.lease.as_deref(),
        event_sink: args.event_sink.as_ref(),
    };
    if Box::pin(reconcile_pending_handoff(&handoff))
        .await?
        .is_some()
    {
        return Ok(NatsAgentLoopOutcome::HandoffDispatched);
    }
    run_unparked_agent_loop(args).await
}

async fn run_unparked_agent_loop(args: RunAgentLoopArgs<'_>) -> Result<NatsAgentLoopOutcome> {
    let (jetstream_ctx, session_origin, attachment_sync, prepare_handoff_fn) =
        prepare_session_for_agent_loop(&args).await?;

    let ctx = setup_agent_loop_context(
        AgentContextParams {
            config: args.config.clone(),
            instance_id: args.instance_id,
            abort_signal: args.abort_signal.clone(),
            token_budget: args.token_budget,
            call_fn: Some(attachment_sync.model_call(args.call_fn)),
            on_tool_round: args.on_tool_round,
            prepare_handoff_fn,
            working_dir: args.working_dir,
        },
        session_origin,
        args.session_id,
    )
    .await?;

    let replicas = attachment_sync.replicas();
    let session_id = args.session_id;
    let segment_args = AgentLoopSegmentArgs {
        source_session_id: session_id,
        cluster_key: args.cluster_key,
        config: args.config,
        ctx,
        input: args.initial_input,
        abort_signal: args.abort_signal,
        jetstream_ctx: jetstream_ctx.clone(),
        activation_route: args.activation_route,
        event_sink: args.event_sink,
        lease: args.lease,
        replicas,
    };
    let result = run_nats_loop_segment(segment_args, jetstream_ctx, session_id, replicas).await;
    attachment_sync.finish(result).await
}

struct PrepareAgentSessionParams<'a> {
    cluster_key: &'a str,
    activation_route: &'a super::SessionActivationRoute,
    session_id: &'a str,
    config: &'a GlobalConfig,
    instance_id: &'a harnx_core::instance::ServerScope,
    abort_signal: &'a AbortSignal,
    lease: Option<&'a Arc<NatsSessionLease>>,
    after_seq_observer: Option<Arc<AtomicU64>>,
    session_metadata: Option<&'a SessionMetadataStore>,
    selected_user_messages: &'a [Message],
    replay_unmanaged: bool,
}

async fn prepare_agent_session(
    params: PrepareAgentSessionParams<'_>,
) -> Result<(
    jetstream::Context,
    SessionOrigin,
    SessionAttachmentSync,
    Arc<crate::tool::PrepareHandoffFn>,
)> {
    let cfg_snapshot = params.config.read().clone();
    let replicas = cfg_snapshot
        .resolve_nats_server(params.cluster_key)
        .await?
        .resolved_replicas();
    let jetstream = cfg_snapshot.nats_jetstream(params.cluster_key).await?;
    let prepare_handoff_fn = handoff::NatsHandoffContext {
        config: params.config.clone(),
        source_session_id: params.session_id.to_string(),
        cluster_key: params.cluster_key.to_string(),
        jetstream_ctx: jetstream.clone(),
        activation_route: params.activation_route.clone(),
        replicas,
    }
    .callback(params.abort_signal.clone());
    let mut backend = NatsSessionLogBackend::new(jetstream.clone(), params.session_id, replicas);
    // The canonical metadata store backs both metadata persistence and the
    // `ToolResults` size bound: an oversized round derives its attachment
    // owner (agent + local session id) from the canonical record.
    backend = backend.with_metadata_store(params.session_metadata.cloned());
    if let Some(observer) = params.after_seq_observer {
        backend = backend.with_after_seq_observer(observer);
    }
    abort_resume_if_fenced(&backend, params.lease.map(Arc::as_ref))?;
    let (mut session, origin, timing_progress) =
        load_or_repair_session(LoadOrRepairSessionParams {
            backend: &backend,
            config: params.config,
            instance_id: params.instance_id,
            lease: params.lease.map(Arc::as_ref),
            session_metadata: params.session_metadata,
            session_id: params.session_id,
            abort_signal: params.abort_signal,
            replay_unmanaged: params.replay_unmanaged,
            prepare_handoff_fn: Some(prepare_handoff_fn.clone()),
        })
        .await?;
    selected_input::restore_selected_user_messages(&mut session, params.selected_user_messages);
    attach_session_to_config(AttachSessionParams {
        config: params.config,
        session: session.clone(),
        backend: &backend,
        lease: params.lease,
        metadata: params.session_metadata,
    });
    crate::config::session_timing::hydrate_run(params.config, timing_progress);
    // Get the SessionRef from the loaded session
    let session_ref = SessionRef::new(
        session
            .agent_name()
            .filter(|name| !name.is_empty() && *name != harnx_core::agent_config::TEMP_AGENT_NAME)
            .map(str::to_string),
        session.id().to_string(),
    )
    .expect("session has valid id");
    let attachment_sync = SessionAttachmentSync::prepare(
        jetstream.clone(),
        params.config.clone(),
        params.cluster_key,
        session_ref,
    )
    .await?;
    Ok((jetstream, origin, attachment_sync, prepare_handoff_fn))
}

struct AgentContextParams {
    prepare_handoff_fn: Arc<crate::tool::PrepareHandoffFn>,
    config: GlobalConfig,
    instance_id: harnx_core::instance::ServerScope,
    abort_signal: AbortSignal,
    token_budget: Option<u64>,
    call_fn: Option<crate::agent_loop::AgentCallFn>,
    on_tool_round: Option<OnToolRoundFn>,
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
    crate::agent_loop::AgentLoopContext {
        config: params.config,
        instance_id: params.instance_id,
        abort_signal: params.abort_signal,
        token_budget: params.token_budget,
        usage_at_start,
        call_fn: params.call_fn,
        on_tool_round: params.on_tool_round,
        on_text_response: None,
        initial_with_embeddings: false,
        initial_resume_count: 0,
        max_resume: None,
        prepare_handoff_fn: Some(params.prepare_handoff_fn),
        pending_async_context: Some(Arc::new(tokio::sync::Mutex::new(None))),
        working_dir: params.working_dir,
        tool_loop_guard: Default::default(),
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
    session_id: &'a str,
    cwd: std::path::PathBuf,
    model: String,
    pending_async_context: Option<Arc<tokio::sync::Mutex<Option<String>>>>,
}

/// Dispatch SessionStart through the in-process event hooks.
///
/// Added context rides the loop's pending queue into the first turn's
/// `hook_context`. A `deny` policy failure propagates: the activation that
/// created the session fails with the hook's error.
async fn dispatch_context_session_start(
    ctx: &crate::agent_loop::AgentLoopContext,
    origin: SessionOrigin,
    session_id: &str,
) -> Result<()> {
    // Same cwd rule as the shared agent loop: the session's working directory
    // when it has one, else the worker's.
    let cwd = ctx
        .working_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let model = ctx.config.read().current_model().id().to_string();
    dispatch_session_start(
        SessionStartDispatch {
            origin,
            session_id,
            cwd,
            model,
            pending_async_context: ctx.pending_async_context.clone(),
        },
        &ctx.config,
        &ctx.abort_signal,
    )
    .await
}

/// Fire SessionStart for a session this worker just created.
///
/// The worker owns this event because it loads the session's resolved scripts.
/// Every activation of an existing session skips it: activations
/// happen once per turn, and the activation that created the session already
/// fired it.
///
/// SessionStart hooks observe and contribute context; the session already
/// exists by the time they run, so there is nothing for a block to prevent —
/// but a `deny` failure stops the activation.
async fn dispatch_session_start(
    params: SessionStartDispatch<'_>,
    config: &GlobalConfig,
    abort: &AbortSignal,
) -> Result<()> {
    if params.origin == SessionOrigin::Resumed {
        return Ok(());
    }
    let outcome = crate::event_hooks::run_event_hooks(
        config,
        &crate::event_hooks::EventHookInput {
            event: harnx_core::hooks::HookEventKind::SessionStart,
            session_id: params.session_id.to_string(),
            cwd: params.cwd,
            resume_count: 0,
            source: Some("startup".to_string()),
            model: Some(params.model),
            ..Default::default()
        },
        abort,
    )
    .await?;
    if let Some(context) = outcome.context_text() {
        if let Some(pending) = &params.pending_async_context {
            let mut queue = pending.lock().await;
            match queue.as_mut() {
                Some(pending) => {
                    pending.push_str("\n\n");
                    pending.push_str(&context);
                }
                None => *queue = Some(context),
            }
        }
    }
    Ok(())
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
    replicas: usize,
}

async fn run_agent_loop_segment(args: AgentLoopSegmentArgs<'_>) -> Result<NatsAgentLoopOutcome> {
    let args_ref = &args;
    let result = Box::pin(crate::agent_loop::run_agent_loop_with_before_end(
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
    ))
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
    let pending_interrupt_ids = continuation.legacy_request_ids;
    let result = crate::agent_loop::continue_agent_loop_from_tool_round(
        &args.ctx,
        args.input.clone(),
        continuation.round,
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
    args.ctx.check_generation("handoff-create")?;
    handoff_dispatch::HandoffDispatchArgs {
        config: &args.config,
        source_session_id: args.source_session_id,
        cluster_key: args.cluster_key,
        jetstream_ctx: &args.jetstream_ctx,
        activation_route: &args.activation_route,
        replicas: args.replicas,
        abort_signal: &args.abort_signal,
        lease: args.lease.as_deref(),
        event_sink: args.event_sink.as_ref(),
    }
    .dispatch(harnx_core::tool::SwitchAgentData {
        agent,
        session_id,
        prompt,
        tool_call_id: handoff_tool_call_id,
    })
    .await
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
    prepare_handoff_fn: Option<Arc<crate::tool::PrepareHandoffFn>>,
    backend: &'a NatsSessionLogBackend,
    config: &'a GlobalConfig,
    instance_id: &'a harnx_core::instance::ServerScope,
    lease: Option<&'a NatsSessionLease>,
    session_metadata: Option<&'a SessionMetadataStore>,
    session_id: &'a str,
    abort_signal: &'a AbortSignal,
    replay_unmanaged: bool,
}

fn recover_session_timing_progress(
    config: &GlobalConfig,
    entries: &[(u64, SessionLogEntry)],
    replay_unmanaged: bool,
) -> Option<harnx_core::timing_identity::TimingReceipt> {
    let cfg = config.read();
    // Only replay may restore an unfinished unmanaged turn's clock.
    if cfg.run_context.is_some() || replay_unmanaged {
        crate::config::session_timing::recover_progress(entries, cfg.run_context.as_ref())
    } else {
        None
    }
}

fn hydrate_session_from_metadata(
    config: &GlobalConfig,
    metadata: &SessionMetadata,
    entries: &[(u64, SessionLogEntry)],
    session_id: &str,
    preserve_hitl_pending: bool,
) -> Result<harnx_core::session::Session> {
    let mut session = crate::config::session::new(&config.read(), &metadata.session_id, None)?;
    session.id = metadata.session_id.clone();
    session.session_id = Some(metadata.session_id.clone());
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
    if preserve_hitl_pending {
        crate::nats_session_log::load_session_from_entries_with_metadata_preserving_pending(
            entries, session_id, session,
        )
    } else {
        crate::nats_session_log::load_session_from_entries_with_metadata(
            entries, session_id, session,
        )
    }
}

/// Load a conversation-only transcript from canonical metadata, repairing
/// orphan tool calls when resuming an interrupted turn.
async fn load_or_repair_session(
    params: LoadOrRepairSessionParams<'_>,
) -> Result<(
    harnx_core::session::Session,
    SessionOrigin,
    Option<harnx_core::timing_identity::TimingReceipt>,
)> {
    let LoadOrRepairSessionParams {
        prepare_handoff_fn,
        backend,
        config,
        instance_id,
        lease,
        session_metadata,
        session_id,
        abort_signal,
        replay_unmanaged,
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
    let timing_progress = recover_session_timing_progress(config, &entries_vec, replay_unmanaged);
    let effective_entries =
        harnx_core::session_reconstruct::apply_log_mutations_nats(&entries_vec)?;
    let preserve_hitl_pending = tool_recovery::preserves_hitl_pending(&effective_entries)
        || has_orphan_tool_calls(&effective_entries);
    repair_orphan_tool_calls_if_any(RepairOrphanCallsParams {
        prepare_handoff_fn,
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
    let session = hydrate_session_from_metadata(
        config,
        &metadata,
        &entries_vec,
        session_id,
        preserve_hitl_pending,
    )?;
    store.mark_activated(session_id).await?;
    Ok((session, origin, timing_progress))
}

struct RepairOrphanCallsParams<'a> {
    prepare_handoff_fn: Option<Arc<crate::tool::PrepareHandoffFn>>,
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
        prepare_handoff_fn,
        backend,
        config,
        instance_id,
        lease,
        session_id,
        abort_signal,
        effective_entries,
        entries_vec,
    } = params;
    let mut orphan_calls = Vec::new();
    for orphan in find_orphan_tool_calls(effective_entries) {
        let hitl_ids = hitl_managed_tool_call_ids(effective_entries, orphan.seq);
        let managed = orphan
            .calls
            .iter()
            .any(|call| call.id.as_deref().is_some_and(|id| hitl_ids.contains(id)));
        if !managed
            && !middleware_recovery::round_needs_reconciliation(backend, config, &orphan).await?
        {
            orphan_calls.push(orphan);
        }
    }
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
            prepare_handoff_fn,
            config: config.clone(),
            instance_id,
            fence_token: lease.map(|l| l.fence_token()),
            worker_id: lease.map(|l| l.worker_id().to_string()),
            lease,
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
/// Returns true if there are any orphan tool calls in the effective entries.
pub(crate) fn has_orphan_tool_calls(
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
) -> bool {
    !find_orphan_tool_calls(entries).is_empty()
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
            SessionLogEntry::ToolResults { .. } | SessionLogEntry::Cancel { .. } => {
                // Cancel closes recovery without authoring late tool output.
                last_tool_calls = None;
            }
            SessionLogEntry::Clear => {
                // A reset abandons every earlier round, including older unmatched
                // batches. Never re-dispatch their effects from retained checkpoints.
                orphans.clear();
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
    let mut eval_ctx = build_orphan_tool_eval_context(
        &args.config,
        args.instance_id,
        &tool_repair,
        args.prepare_handoff_fn.clone(),
    )
    .await;

    for orphan in orphan_calls {
        let results = repair_single_orphan(orphan, &args, &tool_repair, &mut eval_ctx).await?;
        let entry = apply_optional_fence_token(
            SessionLogEntry::ToolResults {
                handoff_selection: None,
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
    prepare_handoff_fn: Option<Arc<crate::tool::PrepareHandoffFn>>,
) -> crate::tool::ToolEvalContext {
    crate::tool::build_tool_eval_context(crate::tool::BuildToolEvalContextParams {
        config,
        instance_id,
        prepare_handoff_fn,
        agent_use_tools: repair.agent_use_tools.as_deref(),
        current_agent_package: repair.current_agent_package.clone(),
        working_dir: None,

        pending_async_context: None,
        // Orphan repair reruns calls outside the originating round; script
        // mode's `bash` must fail closed there rather than redispatch.
        bash_context_slot: None,
        command_bash: None,
    })
    .await
}

fn partition_orphan_calls(
    calls: &[harnx_core::tool::ToolCall],
    args: &RepairOrphanToolCallsArgs<'_>,
    repair: &ToolRepairContext,
) -> (
    Vec<harnx_core::session::ToolOutput>,
    Vec<harnx_core::tool::ToolCall>,
) {
    let mut results = Vec::new();
    let mut rerun_calls = Vec::new();

    for call in calls {
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
    mut rerun_calls: Vec<(usize, harnx_core::tool::ToolCall)>,
    eval_ctx: &crate::tool::ToolEvalContext,
    abort_signal: &AbortSignal,
    authorization: Option<&dyn harnx_core::tool::ReplayAuthorization>,
) -> Result<Vec<(usize, harnx_core::session::ToolOutput)>> {
    for (_, call) in &mut rerun_calls {
        harnx_engine::tool::ensure_tool_call_ids(std::slice::from_mut(call));
    }
    let calls = rerun_calls.iter().map(|(_, call)| call.clone()).collect();
    match harnx_engine::tool::eval_tool_calls_with_authorization(
        eval_ctx,
        calls,
        abort_signal,
        authorization,
    )
    .await
    {
        Ok(tool_results) => Ok(tool_recovery::index_rerun_results(
            rerun_calls,
            tool_results,
        )),
        Err(err) => Ok(rerun_calls
            .into_iter()
            .map(|(index, call)| (index, rerun_failure_output(&call, &err)))
            .collect()),
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
mod tests;

/// NATS-backed tests for `build_mid_turn_injection_callback` (issue #2493).
#[cfg(test)]
#[path = "mid_turn_injection_tests.rs"]
mod mid_turn_injection_tests;

#[cfg(test)]
mod compaction_continuity_tests;

mod selected_input;

mod segment_dispatch;

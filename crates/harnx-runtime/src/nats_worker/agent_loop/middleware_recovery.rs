//! Reconcile middleware-owned calls before entering their normal continuation.
use super::{
    build_orphan_tool_eval_context, build_tool_repair_context, find_orphan_tool_calls,
    hitl_managed_tool_call_ids, partition_orphan_calls, round_continuation, tool_round_entries,
    AgentLoopSegmentArgs, HitlToolRoundContinuation, NatsSessionLogBackend, PendingToolCalls,
    RepairOrphanToolCallsArgs,
};
use crate::config::GlobalConfig;
use crate::interaction_checkpoint::{
    live::DurableRound,
    recovery::{classify, CallRecovery, RoundRecovery},
};
use anyhow::Result;
use harnx_core::session::SessionLogEntry;
use harnx_core::tool::{ToolCall, ToolResult};

pub(in crate::nats_worker) async fn parks_recovery(
    backend: &NatsSessionLogBackend,
    config: &GlobalConfig,
) -> Result<bool> {
    let entries = backend.load_events_latest_async().await?;
    #[cfg(feature = "fault-injection")]
    if crate::fault_injection::t2_1_worker_preflight(backend.session_id()) {
        // Exercise the deployed T2.1 execution gate on real worker activations.
        return crate::interaction_checkpoint::guard::is_parked(
            &backend.jetstream(),
            backend.session_id(),
            &entries,
        )
        .await;
    }
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)?;
    for orphan in find_orphan_tool_calls(&effective) {
        if let Err(error) = Box::pin(classify(config, backend, orphan.seq, &orphan.calls)).await {
            log::warn!(
                "checkpoint recovery deferred: session_id={} round={} error={error:#}",
                backend.session_id(),
                orphan.seq
            );
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn requires_reconciliation(plan: &RoundRecovery) -> bool {
    plan.owned
        || plan
            .calls
            .iter()
            .any(|call| !matches!(call, CallRecovery::Legacy))
}

pub(super) async fn round_needs_reconciliation(
    backend: &NatsSessionLogBackend,
    config: &GlobalConfig,
    orphan: &PendingToolCalls,
) -> Result<bool> {
    Ok(requires_reconciliation(
        &Box::pin(classify(config, backend, orphan.seq, &orphan.calls)).await?,
    ))
}

pub(super) async fn reconcile(
    args: &AgentLoopSegmentArgs<'_>,
    backend: &NatsSessionLogBackend,
) -> Result<Option<HitlToolRoundContinuation>> {
    let entries = backend.load_events_latest_async().await?;
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)?;
    let Some(orphan) = find_orphan_tool_calls(&effective).into_iter().last() else {
        return Ok(None);
    };
    let plan = Box::pin(classify(&args.config, backend, orphan.seq, &orphan.calls)).await?;
    if !requires_reconciliation(&plan) {
        return Ok(None);
    }
    let round = DurableRound::open_recovery(&args.config, orphan.seq, &orphan.calls).await?;
    let repair = build_tool_repair_context(&args.config);
    let mut eval = build_orphan_tool_eval_context(
        &args.config,
        &args.ctx.instance_id,
        &repair,
        args.ctx.prepare_handoff_fn.clone(),
    )
    .await;
    eval.tool_round = Some(orphan.seq);
    let managed = hitl_managed_tool_call_ids(&effective, orphan.seq);
    for (call, state) in orphan.calls.iter().zip(plan.calls) {
        let result = match state {
            CallRecovery::Interrupted(message) => Some(ToolResult::new(
                call.clone(),
                serde_json::json!({"is_error": true, "error": message}),
            )),
            CallRecovery::Legacy if !call.id.as_deref().is_some_and(|id| managed.contains(id)) => {
                recover_legacy_slot(args, &orphan, call, plan.started, &eval).await?
            }
            _ => None,
        };
        if let Some(result) = result {
            round.complete_recovered(result, &eval).await?;
        }
    }
    let entries = backend.load_events_latest_async().await?;
    let decisions = legacy_decisions(&entries, orphan.seq);
    let legacy_request_ids = managed.into_iter().map(str::to_owned).collect();
    Ok(Some(round_continuation(
        orphan,
        decisions,
        legacy_request_ids,
    )))
}

async fn recover_legacy_slot(
    args: &AgentLoopSegmentArgs<'_>,
    orphan: &PendingToolCalls,
    call: &ToolCall,
    started: bool,
    eval: &crate::tool::ToolEvalContext,
) -> Result<Option<ToolResult>> {
    // Only round-start proves this unstarted slot couldn't have dispatched.
    if !started {
        if let Some(result) = harnx_engine::tool::replay_tool_call(
            eval,
            harnx_core::tool::ToolReplay {
                session_id: args.source_session_id,
                tool_round: orphan.seq,
                call,
                worker_id: args.lease.as_ref().map(|l| l.worker_id()),
                fence_token: args.lease.as_ref().map(|l| l.fence_token()),
                authorization: args
                    .lease
                    .as_ref()
                    .map(|l| l.as_ref() as &dyn harnx_core::tool::ReplayAuthorization),
            },
            &args.abort_signal,
        )
        .await?
        {
            return Ok(Some(result));
        }
    }
    let partition_args = RepairOrphanToolCallsArgs {
        prepare_handoff_fn: args.ctx.prepare_handoff_fn.clone(),
        config: args.config.clone(),
        instance_id: &args.ctx.instance_id,
        fence_token: args.lease.as_ref().map(|l| l.fence_token()),
        worker_id: args.lease.as_ref().map(|l| l.worker_id().to_owned()),
        session_id: args.source_session_id,
        abort_signal: &args.abort_signal,
        lease: args.lease.as_deref(),
    };
    let mut repair = build_tool_repair_context(&args.config);
    // A fresh worker has not published discovery into the config cache yet.
    // Classify reruns against the provider snapshot that will execute them.
    if let Some(render) = &eval.render {
        repair.decl_map.extend(
            render
                .decl_map
                .iter()
                .map(|(name, declaration)| (name.clone(), declaration.clone())),
        );
    }
    let (synthetic, _) =
        partition_orphan_calls(std::slice::from_ref(call), &partition_args, &repair);
    Ok(synthetic
        .into_iter()
        .next()
        .map(|output| ToolResult::new(call.clone(), output.output)))
}

fn legacy_decisions(
    entries: &[(u64, SessionLogEntry)],
    round: u64,
) -> Vec<crate::agent_loop::ToolApprovalDecision> {
    let mut requested = std::collections::HashSet::new();
    let mut decisions = std::collections::BTreeMap::new();
    for (_, entry) in tool_round_entries(entries, round) {
        match entry {
            SessionLogEntry::HitlApprovalRequested { tool_call_id, .. } => {
                requested.insert(tool_call_id);
            }
            SessionLogEntry::HitlApprovalDecision {
                tool_call_id,
                approved,
                note,
                ..
            } if requested.contains(tool_call_id) => {
                decisions.insert(
                    tool_call_id,
                    crate::agent_loop::ToolApprovalDecision {
                        tool_call_id: tool_call_id.clone(),
                        approved: *approved,
                        reason: note.clone(),
                    },
                );
            }
            _ => {}
        }
    }
    decisions.into_values().collect()
}

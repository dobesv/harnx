use super::*;

#[cfg(test)]
#[path = "tool_recovery_tests.rs"]
pub(super) mod tests;

#[async_trait::async_trait]
impl harnx_core::tool::ReplayAuthorization for NatsSessionLease {
    async fn revalidate(&self) -> Result<()> {
        anyhow::ensure!(
            self.revalidate_ownership().await?,
            "session lease lost before tool replay"
        );
        Ok(())
    }
}

/// HITL-managed rounds bypass orphan repair, but not original-generation admission.
pub(super) async fn admit_pending_rounds(
    backend: &NatsSessionLogBackend,
    config: &GlobalConfig,
    entries: &[(u64, SessionLogEntry)],
) -> Result<()> {
    let orphans = find_orphan_tool_calls(entries);
    if orphans.is_empty() {
        return Ok(());
    }
    let fence = config
        .read()
        .generation_fence
        .clone()
        .context("unknown pending round authority")?;
    let log =
        crate::nats_session_log::NatsSessionLog::new(backend.jetstream(), backend.session_id());
    for orphan in orphans {
        let original = log
            .entry_authority(&fence.store, orphan.seq, &orphan.calls)
            .await?;
        crate::nats_session_log::recovery::admit_original(&fence, original).await?;
    }
    Ok(())
}

pub(super) async fn repair_single_orphan(
    orphan: &PendingToolCalls,
    args: &RepairOrphanToolCallsArgs<'_>,
    repair: &ToolRepairContext,
    eval_ctx: &crate::tool::ToolEvalContext,
) -> Result<Vec<harnx_core::session::ToolOutput>> {
    let fence = args
        .config
        .read()
        .generation_fence
        .clone()
        .context("unknown orphan recovery authority")?;
    let original = args
        .log
        .entry_authority(&fence.store, orphan.seq, &orphan.calls)
        .await?;
    // BEFORE journal dispatch or the legacy idempotency partition. Stop and owner
    // handover race this admission; idempotence never overrides interruption.
    crate::nats_session_log::recovery::admit_original(&fence, original).await?;
    let mut results = vec![Vec::new(); orphan.calls.len()];
    let mut reruns = Vec::new();
    for (index, call) in orphan.calls.iter().enumerate() {
        match harnx_engine::tool::replay_tool_call(
            eval_ctx,
            harnx_core::tool::ToolReplay {
                session_id: args.session_id,
                tool_round: orphan.seq,
                call,
                worker_id: args.worker_id.as_deref(),
                fence_token: args.fence_token,
                authorization: args
                    .lease
                    .map(|lease| lease as &dyn harnx_core::tool::ReplayAuthorization),
            },
            args.abort_signal,
        )
        .await?
        {
            Some(result) => results[index].push(harnx_core::session::ToolOutput {
                id: result.call.id,
                name: result.call.name,
                output: result.output,
                markdown: result.markdown,
                content: result.content,
                switch_agent: result.switch_agent,
            }),
            None => {
                fence.check("legacy-orphan-partition").await?;
                let (synthetic, rerun) =
                    partition_orphan_calls(std::slice::from_ref(call), args, repair);
                results[index] = synthetic;
                if !rerun.is_empty() {
                    reruns.extend(rerun.into_iter().map(|call| (index, call)));
                }
            }
        }
    }
    recover_legacy_calls(reruns, args, eval_ctx, &mut results).await?;
    fence.check("recovery-results").await?;
    Ok(results.into_iter().flatten().collect())
}

async fn recover_legacy_calls(
    reruns: Vec<(usize, harnx_core::tool::ToolCall)>,
    args: &RepairOrphanToolCallsArgs<'_>,
    eval_ctx: &crate::tool::ToolEvalContext,
    results: &mut [Vec<harnx_core::session::ToolOutput>],
) -> Result<()> {
    if reruns.is_empty() {
        return Ok(());
    }
    let authorization = args
        .lease
        .map(|lease| lease as &dyn harnx_core::tool::ReplayAuthorization);
    if let Some(authorization) = authorization {
        authorization.revalidate().await?;
    }
    // Keep the batch-wide approval barrier while retaining each result's
    // original position across synthetic, saved, and newly dispatched calls.
    let completed =
        rerun_or_synthesize_tool_results(reruns, eval_ctx, args.abort_signal, authorization)
            .await?;
    for (index, output) in completed {
        results[index].push(output);
    }
    Ok(())
}

pub(super) fn index_rerun_results(
    calls: Vec<(usize, harnx_core::tool::ToolCall)>,
    results: Vec<harnx_core::tool::ToolResult>,
) -> Vec<(usize, harnx_core::session::ToolOutput)> {
    let mut positions: Vec<_> = calls.into_iter().map(Some).collect();
    results
        .into_iter()
        .map(|result| {
            // The engine retains the submitted call on every output. Consume one
            // position per result so anonymous duplicate calls remain distinct.
            let slot = positions
                .iter_mut()
                .find(|slot| {
                    slot.as_ref().is_some_and(|(_, call)| {
                        call.id == result.call.id
                            && call.name == result.call.name
                            && call.arguments == result.call.arguments
                    })
                })
                .expect("engine result must correspond to a submitted tool call");
            let (index, _) = slot.take().expect("matched call position");
            (
                index,
                harnx_core::session::ToolOutput {
                    id: result.call.id,
                    name: result.call.name,
                    output: result.output,
                    markdown: result.markdown,
                    content: result.content,
                    switch_agent: result.switch_agent,
                },
            )
        })
        .collect()
}

pub(super) fn preserves_hitl_pending(effective_entries: &[(u64, SessionLogEntry)]) -> bool {
    find_orphan_tool_calls(effective_entries)
        .iter()
        .any(|orphan| {
            let hitl_ids = hitl_managed_tool_call_ids(effective_entries, orphan.seq);
            orphan
                .calls
                .iter()
                .any(|call| call.id.as_deref().is_some_and(|id| hitl_ids.contains(id)))
        })
}

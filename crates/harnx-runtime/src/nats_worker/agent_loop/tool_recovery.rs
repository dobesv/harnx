use super::*;

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

pub(super) async fn repair_single_orphan(
    orphan: &PendingToolCalls,
    args: &RepairOrphanToolCallsArgs<'_>,
    repair: &ToolRepairContext,
    eval_ctx: &crate::tool::ToolEvalContext,
) -> Result<Vec<harnx_core::session::ToolOutput>> {
    let mut recovered = Vec::new();
    let mut remaining = Vec::new();
    for call in &orphan.calls {
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
            Some(result) => recovered.push(harnx_core::session::ToolOutput {
                id: result.call.id,
                name: result.call.name,
                output: result.output,
                markdown: result.markdown,
                content: result.content,
                switch_agent: result.switch_agent,
            }),
            None => remaining.push(call.clone()),
        }
    }
    let (mut results, rerun_calls) = partition_orphan_calls(&remaining, args, repair);
    results.extend(recovered);
    if !rerun_calls.is_empty() {
        let rerun_results =
            rerun_or_synthesize_tool_results(rerun_calls, eval_ctx, args.abort_signal).await;
        results.extend(rerun_results);
    }
    Ok(results)
}

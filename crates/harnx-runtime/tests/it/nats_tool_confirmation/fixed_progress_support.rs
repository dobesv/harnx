use super::*;
use harnx_runtime::{
    nats_session::fixed_admission::{FixedAdmissionOutcome, FixedAdmissionTicket},
    HitlDecisionReceipt, PendingHitlApproval, PromptProgress,
};

pub(super) async fn verify_fixed_prompt_hitl_progress_and_reused_call_id() -> Result<()> {
    let harness = ConfirmationHarness::start()
        .await?
        .context("nats-server required")?;
    let session = harness.source.clone().with_external_admission();
    let ticket = session
        .prepare_fixed_admission(
            "hitl-progress".into(),
            "hitl-progress-prompt".into(),
            "hitl-progress-closed".into(),
        )
        .await?;
    let (prompt_sequence, first) =
        assert_initial_hitl_request(&session, &ticket, &harness.jetstream).await?;
    let log = NatsSessionLog::new_with_replicas(harness.jetstream.clone(), source_key(), 1);
    let ctx = InvocationProgressContext {
        session: &session,
        ticket: &ticket,
        log: &log,
    };
    let receipt = assert_decided_running_receipt(&ctx, &first).await?;
    let round_and_request = assert_next_round_reused_id(&ctx, &first, &receipt).await?;
    assert_terminal_and_next_invocation_isolation(&ctx, prompt_sequence, round_and_request).await
}

struct InvocationProgressContext<'a> {
    session: &'a NatsSession,
    ticket: &'a FixedAdmissionTicket,
    log: &'a NatsSessionLog,
}

async fn append_and_start_follow(
    session: &NatsSession,
    ticket: &FixedAdmissionTicket,
) -> Result<u64> {
    assert!(session.fixed_prompt_progress(ticket).await.is_err());
    let FixedAdmissionOutcome::Admitted { prompt_sequence } = session
        .append_fixed_prompt(ticket, MessageContent::Text("start handoff".into()))
        .await?
    else {
        anyhow::bail!("expected admitted prompt");
    };
    assert!(matches!(
        session.fixed_prompt_progress(ticket).await?,
        PromptProgress::Running {
            last_decision: None
        }
    ));
    let handle = session
        .fixed_prompt_handle(ticket)
        .await?
        .context("prompt handle")?;
    session
        .follow_admitted_prompt(handle, Arc::new(NullSink), None, None, Default::default())
        .await?;
    Ok(prompt_sequence)
}

fn assert_initial_approval_fields(first: &PendingHitlApproval, prompt_sequence: u64) {
    assert_eq!(first.tool_call_id, "hook-approval-handoff");
    assert_eq!(first.tool_name, "target_session_handoff");
    assert_eq!(first.arguments["session_id"], TARGET_SESSION_ID);
    assert_eq!(first.summary, "Approve the handoff?");
    assert!(first.tool_round_seq > prompt_sequence);
}

async fn assert_initial_hitl_request(
    session: &NatsSession,
    ticket: &FixedAdmissionTicket,
    jetstream: &async_nats::jetstream::Context,
) -> Result<(u64, PendingHitlApproval)> {
    let prompt_sequence = append_and_start_follow(session, ticket).await?;
    wait_for_source_count(
        jetstream,
        |entry| matches!(entry, SessionLogEntry::HitlApprovalRequested { .. }),
        1,
    )
    .await?;
    let PromptProgress::InputRequired(first) = session.fixed_prompt_progress(ticket).await? else {
        anyhow::bail!("expected worker's durable approval request");
    };
    assert_initial_approval_fields(&first, prompt_sequence);
    Ok((prompt_sequence, first))
}

async fn assert_decided_running_receipt(
    ctx: &InvocationProgressContext<'_>,
    first: &PendingHitlApproval,
) -> Result<HitlDecisionReceipt> {
    let decision_seq = ctx
        .log
        .append_event_async(&SessionLogEntry::HitlApprovalDecision {
            tool_call_id: first.tool_call_id.clone(),
            approved: true,
            note: Some("audit only".into()),
            fence_token: 1,
        })
        .await?;
    let PromptProgress::Running {
        last_decision: Some(receipt),
    } = ctx.session.fixed_prompt_progress(ctx.ticket).await?
    else {
        anyhow::bail!("decided invocation isn't terminal");
    };
    assert_eq!(
        receipt.request,
        first.approval_ref(ctx.ticket.invocation_id())
    );
    assert_eq!(receipt.decision_seq, decision_seq);
    assert!(receipt.approved);
    assert_eq!(receipt.note.as_deref(), Some("audit only"));
    Ok(receipt)
}

async fn assert_next_round_reused_id(
    ctx: &InvocationProgressContext<'_>,
    first: &PendingHitlApproval,
    receipt: &HitlDecisionReceipt,
) -> Result<(SessionLogEntry, SessionLogEntry, PendingHitlApproval)> {
    ctx.log
        .append_event_async(&SessionLogEntry::ToolResults {
            results: Vec::new(),
            timestamp: None,
        })
        .await?;
    let round = SessionLogEntry::ToolCalls {
        text: String::new(),
        thought: None,
        calls: vec![ToolCall::new(
            "next_tool".into(),
            json!({"fresh": true}),
            Some(first.tool_call_id.clone()),
            None,
        )],
        timestamp: None,
        fence_token: Some(1),
    };
    let round_seq = ctx.log.append_event_async(&round).await?;
    let request = SessionLogEntry::HitlApprovalRequested {
        tool_call_id: first.tool_call_id.clone(),
        summary: "Next approval".into(),
        fence_token: 1,
    };
    let request_seq = ctx.log.append_event_async(&request).await?;
    let PromptProgress::InputRequired(next) = ctx.session.fixed_prompt_progress(ctx.ticket).await?
    else {
        anyhow::bail!("expected next request, not previous decision");
    };
    assert_eq!(next.tool_round_seq, round_seq);
    assert_eq!(next.request_seq, request_seq);
    assert_eq!(next.tool_name, "next_tool");
    assert_eq!(next.arguments, json!({"fresh": true}));
    assert_ne!(
        next.approval_ref(ctx.ticket.invocation_id()),
        receipt.request
    );
    Ok((round, request, next))
}

async fn assert_terminal_progress_result(
    ctx: &InvocationProgressContext<'_>,
    prompt_sequence: u64,
) -> Result<()> {
    ctx.log
        .append_event_async(&SessionLogEntry::TurnEnd {
            through_seq: prompt_sequence,
            fence_token: 1,
            timestamp: None,
            usage: None,
        })
        .await?;
    let PromptProgress::Terminal(result) = ctx.session.fixed_prompt_progress(ctx.ticket).await?
    else {
        anyhow::bail!("terminal must win even with unresolved request");
    };
    assert_eq!(result.user_msg_seq, prompt_sequence);
    assert_eq!(result.user_msg_id, ctx.ticket.prompt_id());
    assert!(!result.was_cancelled);
    Ok(())
}

async fn assert_isolated_next_invocation(
    ctx: &InvocationProgressContext<'_>,
    round: &SessionLogEntry,
    request: &SessionLogEntry,
    next: &PendingHitlApproval,
) -> Result<()> {
    let next_ticket = ctx
        .session
        .prepare_fixed_admission(
            "next-hitl-invocation".into(),
            "next-hitl-prompt".into(),
            "next-hitl-closed".into(),
        )
        .await?;
    ctx.session
        .append_fixed_prompt(&next_ticket, MessageContent::Text("next invocation".into()))
        .await?;
    let later_round_seq = ctx.log.append_event_async(round).await?;
    ctx.log.append_event_async(request).await?;
    assert!(matches!(
        ctx.session.fixed_prompt_progress(ctx.ticket).await?,
        PromptProgress::Terminal(_)
    ));
    let PromptProgress::InputRequired(later) =
        ctx.session.fixed_prompt_progress(&next_ticket).await?
    else {
        anyhow::bail!("next invocation must own its approval");
    };
    assert_eq!(later.tool_round_seq, later_round_seq);
    assert_ne!(
        later.approval_ref(next_ticket.invocation_id()),
        next.approval_ref(ctx.ticket.invocation_id())
    );
    Ok(())
}

async fn assert_terminal_and_next_invocation_isolation(
    ctx: &InvocationProgressContext<'_>,
    prompt_sequence: u64,
    round_and_request: (SessionLogEntry, SessionLogEntry, PendingHitlApproval),
) -> Result<()> {
    let (round, request, next) = round_and_request;
    assert_terminal_progress_result(ctx, prompt_sequence).await?;
    assert_isolated_next_invocation(ctx, &round, &request, &next).await
}

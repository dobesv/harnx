use super::*;
use std::time::Duration;

pub(super) struct SerialApprovalContext<'a> {
    pub fixture: &'a BoundaryFixture,
    pub ticket: &'a harnx_runtime::nats_session::fixed_admission::FixedAdmissionTicket,
    pub sink: &'a Arc<TextSink>,
}

pub(super) async fn assert_first_approval_gate(
    ctx: &SerialApprovalContext<'_>,
    reject_first: bool,
) -> Result<PendingHitlApproval> {
    let FollowBoundary::InputRequired(first) = ctx
        .fixture
        .follow(ctx.ticket, ctx.sink.clone(), None)
        .await?
    else {
        anyhow::bail!("initial segment must suspend");
    };
    assert_eq!(first.tool_call_id, "first-approval");
    assert_eq!(
        &*ctx.sink.0.lock(),
        "before approval",
        "live text chunks must reach the sink"
    );
    let FollowBoundary::InputRequired(still_pending) = ctx
        .fixture
        .follow(ctx.ticket, ctx.sink.clone(), Some(u64::MAX))
        .await?
    else {
        anyhow::bail!("cursor must not hide an undecided old request");
    };
    assert_eq!(still_pending, first);
    assert_eq!(
        ctx.fixture
            .harness
            .source
            .decide_hitl_approval_for(
                &first.approval_ref(ctx.ticket.invocation_id()),
                !reject_first,
                reject_first.then(|| "reject first, continue second".into()),
            )
            .await?,
        HitlDecisionOutcome::Applied
    );
    Ok(first)
}

pub(super) async fn assert_second_approval_gate(
    ctx: &SerialApprovalContext<'_>,
    first: &PendingHitlApproval,
) -> Result<PendingHitlApproval> {
    wait_for_source_count(
        &ctx.fixture.harness.jetstream,
        |entry| matches!(entry, SessionLogEntry::HitlApprovalRequested { .. }),
        2,
    )
    .await?;
    let FollowBoundary::InputRequired(second) = ctx
        .fixture
        .follow(ctx.ticket, ctx.sink.clone(), Some(first.request_seq))
        .await?
    else {
        anyhow::bail!("resumed segment must return Q2, never decided Q1");
    };
    assert_eq!(second.tool_call_id, "second-approval");
    assert_eq!(first.tool_round_seq, second.tool_round_seq);
    assert!(second.request_seq > first.request_seq);
    assert_eq!(
        ctx.fixture
            .harness
            .source
            .decide_hitl_approval_for(&second.approval_ref(ctx.ticket.invocation_id()), true, None)
            .await?,
        HitlDecisionOutcome::Applied
    );
    Ok(second)
}

async fn await_terminal_boundary_with_release(
    ctx: &SerialApprovalContext<'_>,
    request_seq: u64,
) -> Result<FollowBoundary> {
    tokio::time::timeout(
        Duration::from_secs(10),
        ctx.fixture.final_started.notified(),
    )
    .await?;
    let mut activations = ctx
        .fixture
        .harness
        .client
        .subscribe(harnx_runtime::nats_worker::notify_subject("local"))
        .await?;
    ctx.fixture.harness.client.flush().await?;
    let follow = ctx
        .fixture
        .follow(ctx.ticket, ctx.sink.clone(), Some(request_seq));
    let release = async {
        tokio::time::timeout(Duration::from_secs(5), activations.next())
            .await?
            .context("resumed follower activation")?;
        ctx.fixture.finish.notify_one();
        Ok::<_, anyhow::Error>(())
    };
    let (boundary, _) = tokio::try_join!(follow, release)?;
    Ok(boundary)
}

fn assert_terminal_result_and_sink(
    ctx: &SerialApprovalContext<'_>,
    result: &harnx_runtime::nats_session::NatsTurnResult,
) {
    assert_eq!(result.user_msg_id, ctx.ticket.prompt_id());
    assert_eq!(result.response.as_deref(), Some("after approvals"));
    assert!(!result.was_cancelled);
    assert_eq!(&*ctx.sink.0.lock(), "before approvalafter approvals");
}

pub(super) async fn assert_terminal_follow_segment(
    ctx: &SerialApprovalContext<'_>,
    second: &PendingHitlApproval,
    reject_first: bool,
) -> Result<()> {
    let boundary = await_terminal_boundary_with_release(ctx, second.request_seq).await?;
    let FollowBoundary::Terminal(result) = boundary else {
        anyhow::bail!("final segment must terminate");
    };
    assert_terminal_result_and_sink(ctx, &result);
    assert!(
        matches!(
            ctx.fixture
                .follow(ctx.ticket, ctx.sink.clone(), Some(u64::MAX))
                .await?,
            FollowBoundary::Terminal(_)
        ),
        "terminal detection must ignore request cursor"
    );
    verify_session_entries(&ctx.fixture.harness.jetstream, ctx.ticket, reject_first).await
}

pub(super) async fn verify_session_entries(
    js: &async_nats::jetstream::Context,
    _ticket: &harnx_runtime::nats_session::fixed_admission::FixedAdmissionTicket,
    reject_first: bool,
) -> Result<()> {
    let entries = NatsSessionLog::new(js.clone(), source_key())
        .load_events_async()
        .await?;
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(
                entry,
                SessionLogEntry::Message { role, content, .. }
                    if role.is_user() && !harnx_core::timing_identity::is_timing_notice(&content.to_text())
            ))
            .count(),
        1,
        "same invocation resumes without another prompt: {entries:?}"
    );
    if reject_first {
        assert!(entries.iter().any(|(_, entry)| matches!(
            entry,
            SessionLogEntry::ToolResults { results, .. }
                if results.iter().any(|result| result.output.to_string().contains("reject first, continue second"))
        )));
    }
    Ok(())
}

use super::*;
use harnx_a2a_server::store::context::ReplyReceipt;
use harnx_a2a_server::store::message_fingerprint;
use harnx_runtime::nats_session::fixed_admission::FixedAdmissionTicket;
use harnx_runtime::PendingHitlApproval;

pub(super) async fn parked(h: &Harness, session: &NatsSession) -> Result<ContextSnapshot> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let context = h
                .store
                .read_context(session.storage_key())
                .await?
                .context("context")?;
            if context.document.owner.is_none()
                && !h
                    .runner
                    .is_busy(&ContextKey::new(&h.export, session.session_id()))
                    .await
            {
                return Ok(context);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .context("park deadline")?
}

pub(super) fn ticket(context: &ContextSnapshot, storage: &str) -> Result<FixedAdmissionTicket> {
    let admission = &context
        .document
        .state
        .active
        .as_ref()
        .context("active")?
        .admission;
    FixedAdmissionTicket::from_parts(
        storage.into(),
        admission.invocation_id.clone(),
        admission.prompt_id.clone(),
        admission.closure_id.clone().context("closure")?,
        admission.fixed_predecessor,
    )
}

pub(super) async fn challenge(
    h: &Harness,
    session: &NatsSession,
) -> Result<(harnx_a2a_server::runner::StartTurnResult, ContextSnapshot)> {
    let mut started = h.send(session, prompt()).await?;
    loop {
        let next = event(&mut started.events).await?;
        if next.ends_response() {
            assert!(!next.is_terminal());
            let StreamResponse::StatusUpdate(update) = next.response else {
                unreachable!()
            };
            assert_eq!(update.status.state, TaskState::InputRequired);
            let message = update.status.message.context("challenge message")?;
            assert_eq!(message.role, Role::Agent);
            assert!(message
                .parts
                .iter()
                .all(|part| matches!(part.content, PartContent::Text(_))));
            let challenge = text(&message.parts);
            assert!(challenge.contains("Approve handoff?"), "{challenge}");
            assert!(
                challenge.contains("Reply `yes`") || challenge.contains("Reply `y`"),
                "{challenge}"
            );
            break;
        }
    }
    let context = parked(h, session).await?;
    Ok((started, context))
}

async fn claim_with_lease(
    h: &Harness,
    session: &NatsSession,
    lease: &NatsSessionLease,
    op: &str,
) -> Result<ContextSnapshot> {
    let identity = ContextIdentity {
        storage_key: session.storage_key(),
        local_id: session.session_id(),
    };
    let write = h.store.prepare_context_claim(identity, lease, op).await?;
    h.store.commit_context(&write).await
}

async fn release_with_lease(
    h: &Harness,
    session: &NatsSession,
    operation: (&ContextVersion, &str),
    lease: NatsSessionLease,
) -> Result<()> {
    let write = h
        .store
        .prepare_context_release(session.storage_key(), operation.0, operation.1)
        .await?;
    h.store.commit_context(&write).await?;
    lease.release().await
}

async fn acquire_fixture_lease(
    js: &async_nats::jetstream::Context,
    storage_key: &str,
    worker_id: &str,
    error_context: &'static str,
) -> Result<NatsSessionLease> {
    NatsSessionLease::acquire_scoped(
        NatsLeaseAcquireParams {
            jetstream: js.clone(),
            session_id: storage_key,
            worker_id: worker_id.into(),
            generation: 1,
            config: NatsLeaseConfig {
                replicas: 1,
                ..Default::default()
            },
            session_metadata: None,
        },
        "a2a",
    )
    .await?
    .context(error_context)
}

async fn advance_following_step(
    h: &Harness,
    session: &NatsSession,
    owned: &ContextSnapshot,
    context: &ContextSnapshot,
) -> Result<ContextSnapshot> {
    let request = &context
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .hitl_checkpoint
        .as_ref()
        .unwrap()
        .request;
    h.store
        .advance_hitl_to_following(
            session.storage_key(),
            &owned.version()?,
            harnx_a2a_server::store::context::HitlAdvanceParams {
                expected_approval: request,
                outcome: &HitlDecisionOutcome::Applied,
            },
        )
        .await?;
    let current = h.store.read_context(session.storage_key()).await?.unwrap();
    Ok(current)
}

pub(super) async fn following(
    h: &Harness,
    session: &NatsSession,
    context: &ContextSnapshot,
) -> Result<()> {
    let lease = acquire_fixture_lease(
        &h.jetstream,
        session.storage_key(),
        "test-following",
        "scoped lease",
    )
    .await?;
    let owned = claim_with_lease(h, session, &lease, "resume-claim").await?;
    let current = advance_following_step(h, session, &owned, context).await?;
    release_with_lease(h, session, (&current.version()?, "resume-release"), lease).await
}

pub(super) async fn wait_terminal_progress(
    session: &NatsSession,
    ticket: &FixedAdmissionTicket,
) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if matches!(
                session.fixed_prompt_progress(ticket).await?,
                PromptProgress::Terminal(_)
            ) {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .context("runtime completion deadline")?
}

pub(super) async fn wait_second_approval(
    h: &Harness,
    session: &NatsSession,
    ticket: &FixedAdmissionTicket,
    first_seq: u64,
    log_text: &str,
) -> Result<PendingHitlApproval> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            match session.fixed_prompt_progress(ticket).await? {
                PromptProgress::InputRequired(pending) if pending.request_seq > first_seq => {
                    return Ok::<_, anyhow::Error>(pending)
                }
                PromptProgress::Terminal(result) => {
                    anyhow::bail!("expected second approval, got terminal {result:?}; {log_text}")
                }
                _ => {}
            }
            let entries = NatsSessionLog::new(h.jetstream.clone(), session.storage_key())
                .load_events_latest_required_async()
                .await?;
            if let Some(pending) =
                harnx_runtime::interactions::derive_pending_interactions(&entries)?
                    .into_iter()
                    .find(|p| p.request_seq > first_seq)
            {
                return Ok::<_, anyhow::Error>(PendingHitlApproval {
                    request_seq: pending.request_seq,
                    tool_round_seq: pending.tool_round_seq,
                    tool_call_id: pending.tool_call_id,
                    tool_name: pending.request.producer,
                    arguments: pending.request.payload,
                    summary: pending.request.summary,
                    interaction_id: Some(pending.interaction_id),
                });
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .context("second approval deadline")?
}

async fn write_saved_intent(
    h: &Harness,
    session: &NatsSession,
    owned: &ContextSnapshot,
    data: (&ApprovalRef, &a2a_lf::Message),
) -> Result<ContextSnapshot> {
    let (request, message) = data;
    let receipt = ReplyReceipt::new(harnx_a2a_server::store::context::ReplyReceiptParams {
        message_id: message.message_id.clone(),
        fingerprint: message_fingerprint(&message.parts),
        request: request.clone(),
        approved: false,
        note: Some("saved full reason".into()),
    });
    let write = h
        .store
        .prepare_context_update_checked(
            harnx_a2a_server::store::context::ContextMutation {
                storage_key: session.storage_key(),
                expected: &owned.version()?,
                operation_id: "seed-reply",
            },
            |state| {
                let active = state.active.as_mut().unwrap();
                let checkpoint = active.hitl_checkpoint.as_mut().unwrap();
                checkpoint.phase = HitlPhase::DecisionIntent;
                checkpoint.reply = Some(receipt);
                active
                    .snapshot
                    .task
                    .history
                    .as_mut()
                    .unwrap()
                    .push(message.clone());
                Ok(())
            },
        )
        .await?;
    h.store.commit_context(&write).await
}

pub(super) async fn seed_saved_intent(
    h: &Harness,
    session: &NatsSession,
    request: &ApprovalRef,
    message: &a2a_lf::Message,
) -> Result<()> {
    let lease =
        acquire_fixture_lease(&h.jetstream, session.storage_key(), "seed-intent", "lease").await?;
    let owned = claim_with_lease(h, session, &lease, "seed-claim").await?;
    let updated = write_saved_intent(h, session, &owned, (request, message)).await?;
    release_with_lease(h, session, (&updated.version()?, "seed-release"), lease).await
}

pub(super) async fn decide_hitl(
    h: &Harness,
    session: &NatsSession,
    target: &ApprovalRef,
    approved: bool,
    note: Option<String>,
) -> Result<HitlDecisionOutcome> {
    let entries = NatsSessionLog::new(h.jetstream.clone(), session.storage_key())
        .load_events_latest_required_async()
        .await?;
    let interaction = entries.iter().find_map(|(seq, entry)| match entry {
        SessionLogEntry::InteractionRequested {
            tool_round_seq,
            tool_call_id,
            interaction_id,
            ..
        } if *seq == target.request_seq
            && *tool_round_seq == target.tool_round_seq
            && *tool_call_id == target.tool_call_id =>
        {
            Some((*seq, interaction_id.clone()))
        }
        _ => None,
    });
    if let Some((request_seq, interaction_id)) = interaction {
        let response = harnx_core::interaction::InteractionResponse {
            action: if approved { "approve" } else { "deny" }.into(),
            data: Default::default(),
            note: note.clone(),
        };
        let outcome = session
            .decide_interaction(&interaction_id, request_seq, &response)
            .await?;
        Ok(match outcome {
            harnx_runtime::interactions::InteractionDecisionOutcome::Applied => {
                HitlDecisionOutcome::Applied
            }
            harnx_runtime::interactions::InteractionDecisionOutcome::AlreadyApplied => {
                HitlDecisionOutcome::AlreadyApplied { approved, note }
            }
            harnx_runtime::interactions::InteractionDecisionOutcome::Conflicting => {
                HitlDecisionOutcome::Conflicting
            }
            harnx_runtime::interactions::InteractionDecisionOutcome::WrongSequence
            | harnx_runtime::interactions::InteractionDecisionOutcome::Stale
            | harnx_runtime::interactions::InteractionDecisionOutcome::AfterComplete => {
                HitlDecisionOutcome::Stale
            }
            harnx_runtime::interactions::InteractionDecisionOutcome::LeaseLost => {
                anyhow::bail!("interaction decision lease lost")
            }
        })
    } else {
        session
            .decide_hitl_approval_for(target, approved, note)
            .await
    }
}

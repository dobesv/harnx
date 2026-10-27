use super::*;

async fn assert_checkpoint_rejected(f: &Fixture, c: &ContextSnapshot) -> Result<()> {
    assert!(f
        .a
        .record_hitl_reply_intent(
            STORAGE,
            &c.version()?,
            HitlReplyIntent {
                expected_approval: &request(20),
                receipt: receipt(20),
            },
        )
        .await
        .is_err());
    assert!(f
        .a
        .advance_hitl_to_following(
            STORAGE,
            &c.version()?,
            HitlAdvanceParams {
                expected_approval: &request(20),
                outcome: &HitlDecisionOutcome::Applied,
            },
        )
        .await
        .is_err());
    unchanged(f, c).await?;
    Ok(())
}

pub(super) async fn assert_missing_checkpoint(
    f: &Fixture,
    c: &ContextSnapshot,
) -> Result<ContextSnapshot> {
    assert_checkpoint_rejected(f, c).await?;
    let w =
        f.a.prepare_context_update(STORAGE, &c.version()?, "legacy-input-required", |s| {
            s.active.as_mut().unwrap().snapshot.task.status.state = TaskState::InputRequired;
        })
        .await?;
    let c = f.a.commit_context(&w).await?;
    assert!(f
        .a
        .record_hitl_reply_intent(
            STORAGE,
            &c.version()?,
            HitlReplyIntent {
                expected_approval: &request(20),
                receipt: receipt(20),
            },
        )
        .await
        .is_err());
    assert!(f
        .a
        .advance_hitl_to_following(
            STORAGE,
            &c.version()?,
            HitlAdvanceParams {
                expected_approval: &request(20),
                outcome: &HitlDecisionOutcome::Applied,
            },
        )
        .await
        .is_err());
    unchanged(f, &c).await?;
    Ok(c)
}

pub(super) async fn assert_wrong_approvals(f: &Fixture, c: &ContextSnapshot) -> Result<()> {
    for wrong in [
        ApprovalRef {
            invocation_id: "wrong".into(),
            ..request(20)
        },
        ApprovalRef {
            tool_round_seq: 13,
            ..request(20)
        },
        ApprovalRef {
            tool_call_id: "wrong".into(),
            ..request(20)
        },
        request(21),
    ] {
        assert!(f
            .a
            .record_hitl_reply_intent(
                STORAGE,
                &c.version()?,
                HitlReplyIntent {
                    expected_approval: &wrong,
                    receipt: receipt(20),
                },
            )
            .await
            .is_err());
        assert!(f
            .a
            .advance_hitl_to_following(
                STORAGE,
                &c.version()?,
                HitlAdvanceParams {
                    expected_approval: &wrong,
                    outcome: &HitlDecisionOutcome::Applied,
                },
            )
            .await
            .is_err());
        unchanged(f, c).await?;
    }
    Ok(())
}

pub(super) async fn assert_wrong_receipt_and_clear(f: &Fixture, c: &ContextSnapshot) -> Result<()> {
    assert!(f
        .a
        .record_hitl_reply_intent(
            STORAGE,
            &c.version()?,
            HitlReplyIntent {
                expected_approval: &request(20),
                receipt: receipt(21),
            },
        )
        .await
        .is_err());
    assert!(f
        .a
        .clear_hitl_checkpoint(STORAGE, &c.version()?)
        .await
        .is_err());
    unchanged(f, c).await?;
    // Raw authority writes cannot smuggle a mismatched receipt past schema validation.
    assert!(f
        .a
        .prepare_context_update(STORAGE, &c.version()?, "wrong-receipt", |state| {
            state
                .active
                .as_mut()
                .unwrap()
                .hitl_checkpoint
                .as_mut()
                .unwrap()
                .reply = Some(receipt(21));
        })
        .await
        .is_err());
    unchanged(f, c).await
}

pub(super) async fn assert_intent_phase_mismatches(f: &Fixture, c: &ContextSnapshot) -> Result<()> {
    let mut opposite = receipt(20);
    opposite.approved = false;
    assert!(f
        .b
        .record_hitl_reply_intent(
            STORAGE,
            &c.version()?,
            HitlReplyIntent {
                expected_approval: &request(20),
                receipt: opposite,
            },
        )
        .await
        .is_err());
    for outcome in [
        HitlDecisionOutcome::Stale,
        HitlDecisionOutcome::Conflicting,
        HitlDecisionOutcome::AlreadyApplied {
            approved: false,
            note: None,
        },
    ] {
        assert!(f
            .a
            .advance_hitl_to_following(
                STORAGE,
                &c.version()?,
                HitlAdvanceParams {
                    expected_approval: &request(20),
                    outcome: &outcome,
                },
            )
            .await
            .is_err());
    }
    unchanged(f, c).await
}

pub(super) async fn assert_following_receipt_immutable(
    f: &Fixture,
    c: &ContextSnapshot,
) -> Result<()> {
    assert!(waiting(f, c, 30).await.is_err());
    assert!(f
        .a
        .prepare_context_update(STORAGE, &c.version()?, "clear-inline", |s| {
            s.active
                .as_mut()
                .unwrap()
                .hitl_checkpoint
                .as_mut()
                .unwrap()
                .reply = None;
        })
        .await
        .is_err());
    assert!(f
        .a
        .prepare_context_update(STORAGE, &c.version()?, "replace-inline", |s| {
            s.active
                .as_mut()
                .unwrap()
                .hitl_checkpoint
                .as_mut()
                .unwrap()
                .reply
                .as_mut()
                .unwrap()
                .message_id = "different".into();
        })
        .await
        .is_err());
    unchanged(f, c).await
}

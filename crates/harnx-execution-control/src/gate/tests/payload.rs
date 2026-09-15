use super::*;

#[tokio::test]
async fn chunked_output_is_exact_and_retained_after_stop_and_checkpoint() -> Result<()> {
    let (_server, store, other) = stores().await?;
    let ctx = root(&store).await?;
    let payload = json!({"text": "large completion".repeat(32 * 1024)});
    let receipt = store
        .commit_blob_output(
            &ctx,
            CommittedOutput {
                id: "model".into(),
                kind: OutputKind::ModelResponse,
                payload: payload.clone(),
            },
        )
        .await?;
    other.interrupt(&scope(&ctx), "stop").await?;
    store.checkpoint_gate(ctx.gate_root()).await?;
    assert_eq!(store.committed_output_payload(&receipt).await?, payload);
    assert!(store
        .commit_blob_output(
            &ctx,
            CommittedOutput {
                id: "late".into(),
                kind: OutputKind::ModelResponse,
                payload
            }
        )
        .await
        .unwrap_err()
        .is::<Interrupted>());
    Ok(())
}

#[tokio::test]
async fn cancellation_projection_is_control_not_output_and_cannot_cover_g2() -> Result<()> {
    let (_server, store, other) = stores().await?;
    let ctx = root(&store).await?;
    let projection = action(
        "cancel",
        GateAction::RecordCancellation {
            through_seq: 3,
            fence_token: 3,
        },
    );
    assert!(store
        .commit_if_admissible(&ctx, projection.clone())
        .await
        .is_err());
    other.interrupt(&scope(&ctx), "stop").await?;
    store.commit_if_admissible(&ctx, projection).await?;
    let next = replace(&store, &ctx).await?;
    assert!(store
        .commit_if_admissible(
            &ctx,
            action(
                "late-cancel",
                GateAction::RecordCancellation {
                    through_seq: 8,
                    fence_token: 3
                }
            )
        )
        .await
        .is_err());
    store
        .commit_if_admissible(&next, output("new", OutputKind::Transcript))
        .await?;
    Ok(())
}

#[tokio::test]
async fn stopped_session_handover_only_authorizes_cancel_projection() -> Result<()> {
    let (_server, store, _) = stores().await?;
    let root = store.session("handover", None, Some("g1")).await?;
    store.claim(&root.reference, owner(1)).await?;
    let ctx = store.activate_gate(&root.reference).await?;
    store
        .cancel_operation(&root.reference, Some("stop"), false)
        .await?;
    store.claim(&root.reference, owner(2)).await?;
    let next = store.activate_gate(&root.reference).await?;
    assert_eq!(next.owner(), &owner(2));
    store
        .commit_if_admissible(
            &next,
            action(
                "cancel-new-owner",
                GateAction::RecordCancellation {
                    through_seq: 2,
                    fence_token: 3,
                },
            ),
        )
        .await?;
    assert!(store
        .commit_if_admissible(&next, output("late", OutputKind::Transcript))
        .await
        .unwrap_err()
        .is::<Interrupted>());
    assert!(store
        .commit_if_admissible(
            &ctx,
            action(
                "cancel-old-owner",
                GateAction::RecordCancellation {
                    through_seq: 3,
                    fence_token: 3
                }
            )
        )
        .await
        .is_err());
    Ok(())
}

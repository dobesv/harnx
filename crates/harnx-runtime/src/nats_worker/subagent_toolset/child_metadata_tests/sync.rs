//! Synchronous `session_prompt` coverage for the `metadata` argument:
//! checkpoint guards, no-checkpoint rejection, and admitted replay that never
//! resets later child edits — including busy-join replay after attachment
//! loss.
use super::*;

/// A synchronous patch-bearing prompt to `session_id`, composed the way the
/// tool server composes one for the review scenarios.
fn sync_patch_params(session_id: &str, context: ToolInvocationContext) -> PromptParams {
    PromptParams {
        content: MessageContent::Text("work".into()),
        session_id: Some(session_id.into()),
        parent_session_id: Some("trusted-parent".into()),
        tool_call_id: None,
        timeout_secs: None,
        cancel: CancellationToken::new(),
        context,
        metadata: PromptMetadata::Apply(patch()),
    }
}

/// An admitted synchronous replay returns the original response even though
/// the original attachment is gone, without resetting a later child edit.
#[tokio::test]
async fn nats_sync_replay_after_attachment_loss_returns_original_response() -> Result<()> {
    let fixture = fixture().await?;
    let session_id = "replay-meta";
    fixture
        .toolset
        .create_session(Some(session_id.into()), None, None)
        .await?;

    let gone_attachment =
        "cid:media:parent/source/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let original = MessageContent::Array(vec![
        MessageContentPart::Text {
            text: "original prompt".into(),
        },
        MessageContentPart::ImageUrl {
            image_url: harnx_core::message::ImageUrl {
                url: gone_attachment.into(),
            },
        },
    ]);
    let won = fixture
        .seed_admission(session_id, "sync-call", original.clone())
        .await?;
    assert!(won, "idle session admits the seed");
    // qual:allow(dry, fragment) reason: "The two sync-replay tests mirror each other by design (admission-bound vs busy-join binding) and share the bind-edit-retry tail; they diverge in what each seeds before it."
    fixture
        .commit_replayed_turn(session_id, "sync-call", original.clone())
        .await?;

    // The retry carries the same call id, the gone attachment and the same
    // patch. Composing fresh would fail on the missing attachment; the
    // admitted replay must not try.
    let (edited, args) = fixture
        .edited_child_retry_args(session_id, gone_attachment)
        .await?;
    fixture
        .assert_sync_replay_never_resets(args, "sync-call", session_id, edited.revision)
        .await?;
    Ok(())
}

/// A patch-bearing synchronous call with no durable invocation checkpoint
/// fails before any mutation, and an explicit-ID create with no checkpoint
/// creates nothing.
#[tokio::test]
async fn nats_sync_patch_without_checkpoint_fails_before_mutation() -> Result<()> {
    let fixture = fixture().await?;
    fixture
        .toolset
        .create_session(Some("nocp-meta".into()), None, None)
        .await?;

    let params = |session_id: &str| PromptParams {
        content: MessageContent::Text("work".into()),
        session_id: Some(session_id.into()),
        parent_session_id: Some("parent-x".into()),
        tool_call_id: None,
        timeout_secs: None,
        cancel: CancellationToken::new(),
        context: ToolInvocationContext {
            call_id: "no-store-call".into(),
            invoking_session_id: Some("parent-x".into()),
            run_context: Some(crate::nats_session::test_support::inherited_tool_context()),
            ..Default::default()
        },
        metadata: PromptMetadata::Apply(patch()),
    };

    let Err(error) = termination::run_prompt(&fixture.toolset, params("nocp-meta")).await else {
        bail!("no checkpoint store must fail");
    };
    assert!(error.to_string().contains("durable invocation checkpoint"));

    let record = fixture.record("nocp-meta").await?.expect("record exists");
    assert_eq!(record.revision, 1, "existing record untouched");
    assert!(record.metadata.title.value.is_none());
    assert_eq!(
        fixture.events("nocp-meta").await?.len(),
        0,
        "nothing appended"
    );

    // An explicit-ID create without a checkpoint creates nothing at all.
    let Err(_error) = termination::run_prompt(&fixture.toolset, params("nocp-create")).await else {
        bail!("no checkpoint store must fail");
    };
    assert!(
        fixture.record("nocp-create").await?.is_none(),
        "no record created"
    );
    assert!(
        fixture.events("nocp-create").await?.is_empty(),
        "no session activity"
    );
    Ok(())
}

/// The synchronous guard checkpoint pins the revision, applies the patch
/// once, and a retry with the saved guard proceeds as AlreadyHeld without
/// writing again.
#[tokio::test]
async fn nats_sync_prepare_pins_revision_and_applies_once() -> Result<()> {
    let fixture = fixture().await?;
    fixture
        .toolset
        .create_session(Some("prep-meta".into()), None, None)
        .await?;
    let request = fixture
        .request(
            "prep-call",
            json!({"message": "hi", "session_id": "prep-meta"}),
        )
        .await?;
    let before = fixture.record("prep-meta").await?.expect("record exists");

    let context = fixture.context(&request).await?;
    let plan = child_metadata::prepare_sync(&fixture.toolset, &context, Some("prep-meta"), patch())
        .await?;
    assert!(matches!(plan, MetadataPlan::Settled), "patch applied once");
    let applied = fixture.record("prep-meta").await?.expect("record exists");

    // The sync guard checkpoint names the session and pins the revision, and
    // carries no async dispatch-only fields.
    let checkpoint = fixture.checkpoint(&request).await?;
    assert_eq!(checkpoint["session_id"], "prep-meta");
    assert_eq!(checkpoint["storage_key"], fixture.storage("prep-meta"));
    assert_eq!(
        checkpoint["metadata_revision"], before.revision,
        "pinned revision"
    );
    assert!(checkpoint.get("prompt_id").is_none(), "sync shape");

    // The retry reads the saved guard and must proceed as AlreadyHeld.
    let retry_context = fixture.context(&request).await?;
    let plan =
        child_metadata::prepare_sync(&fixture.toolset, &retry_context, Some("prep-meta"), patch())
            .await?;
    assert!(
        matches!(plan, MetadataPlan::Settled),
        "AlreadyHeld proceeds"
    );
    let record = fixture.record("prep-meta").await?.expect("record exists");
    assert_eq!(record.revision, applied.revision, "retry wrote nothing");
    assert_patch_applied(&record);
    Ok(())
}

/// A patch-bearing synchronous call with a real durable journal but a missing
/// or malformed inherited run context fails BEFORE the metadata gate: no guard
/// checkpoint, no canonical revision change, no executable append (review
/// F02). Direct/MCP clients build contexts exactly this way.
#[tokio::test]
async fn nats_sync_invalid_run_context_fails_before_metadata_mutation() -> Result<()> {
    let fixture = fixture().await?;
    let (before, request, mut missing) =
        fixture.seeded_patch_context("ctx-meta", "ctx-call").await?;
    missing.run_context = None;
    let error = fixture
        .toolset
        .session_prompt(request.args.clone(), CancellationToken::new(), missing)
        .await
        .expect_err("missing run context must fail");
    assert!(
        error.to_string().contains("requires inherited run context"),
        "got: {error}"
    );

    let mut malformed = fixture.context(&request).await?;
    malformed.run_context = Some(harnx_toolset::AutonomousRunContext {
        snapshot: json!({"bogus": true}),
        started_at_ms: 1,
    });
    let error = fixture
        .toolset
        .session_prompt(request.args.clone(), CancellationToken::new(), malformed)
        .await
        .expect_err("malformed run context must fail");
    assert!(
        error.to_string().contains("invalid inherited run context"),
        "got: {error}"
    );

    let mut bad_time = fixture.context(&request).await?;
    let snapshot = bad_time
        .run_context
        .as_ref()
        .expect("journaled context carries run context")
        .snapshot
        .clone();
    bad_time.run_context = Some(harnx_toolset::AutonomousRunContext {
        snapshot,
        started_at_ms: u64::MAX,
    });
    let error = fixture
        .toolset
        .session_prompt(request.args.clone(), CancellationToken::new(), bad_time)
        .await
        .expect_err("out-of-range started_at_ms must fail");
    assert!(
        error.to_string().contains("number too large"),
        "got: {error}"
    );

    // None of the rejected attempts reached the gate: the record, the
    // invocation checkpoint and the child log are untouched.
    let record = fixture.record("ctx-meta").await?.expect("record exists");
    assert_eq!(record.revision, before.revision, "no revision change");
    assert_eq!(record.metadata, before.metadata, "no mutation");
    assert!(
        fixture
            .journal
            .get(&request)
            .await?
            .context("journal row missing")?
            .checkpoint
            .is_none(),
        "no guard checkpoint written"
    );
    assert_eq!(
        fixture.events("ctx-meta").await?.len(),
        0,
        "nothing appended"
    );
    Ok(())
}

/// An explicit-ID create whose call has a real durable checkpoint store but
/// no inherited run context creates nothing and pins no creation guard
/// (review F02).
#[tokio::test]
async fn nats_sync_invalid_run_context_create_fails_without_mutation() -> Result<()> {
    let fixture = fixture().await?;
    let create = fixture
        .request(
            "ctx-create-call",
            json!({
                "message": "work",
                "session_id": "ctx-create",
                "metadata": patch_json(),
            }),
        )
        .await?;
    let mut context = fixture.context(&create).await?;
    context.run_context = None;
    let Err(error) =
        termination::run_prompt(&fixture.toolset, sync_patch_params("ctx-create", context)).await
    else {
        bail!("missing run context must fail the create too");
    };
    assert!(error.to_string().contains("requires inherited run context"));
    assert!(
        fixture.record("ctx-create").await?.is_none(),
        "no record created"
    );
    assert!(
        fixture
            .journal
            .get(&create)
            .await?
            .context("journal row missing")?
            .checkpoint
            .is_none(),
        "no creation guard written"
    );
    assert!(
        fixture.events("ctx-create").await?.is_empty(),
        "no session activity"
    );
    Ok(())
}

/// A patch-bearing synchronous replay whose saved invocation checkpoint no
/// longer matches this worker's route scope (a same-broker cluster-label
/// change) fails before the gate: the pinned revision is never CAS'd (review
/// F03, synchronous path).
#[tokio::test]
async fn nats_sync_stale_saved_checkpoint_scope_fails_before_metadata_mutation() -> Result<()> {
    let fixture = fixture().await?;
    let (before, request) = fixture
        .seeded_patch_request("sync-scope-meta", "sync-scope-call")
        .await?;
    // The checkpoint this invocation pinned under its original route, before
    // the cluster label changed: the revision guard alone stays valid.
    fixture
        .journal
        .checkpoint_store(&request)
        .checkpoint(json!({
            "session_id": "sync-scope-meta",
            "storage_key": fixture.storage("sync-scope-meta"),
            "cluster": "elsewhere",
            "metadata_revision": before.revision,
        }))
        .await?;
    let context = fixture.context(&request).await?;

    let error = fixture
        .toolset
        .session_prompt(request.args.clone(), CancellationToken::new(), context)
        .await
        .expect_err("stale saved checkpoint scope must fail");
    assert!(
        error
            .to_string()
            .contains("sub-agent checkpoint scope mismatch"),
        "got: {error}"
    );

    let record = fixture
        .record("sync-scope-meta")
        .await?
        .expect("record exists");
    assert_eq!(record.revision, before.revision, "no CAS on stale scope");
    assert_eq!(record.metadata, before.metadata, "no mutation");
    let checkpoint = fixture.checkpoint(&request).await?;
    assert_eq!(
        checkpoint["cluster"], "elsewhere",
        "the failed call did not rewrite the first writer"
    );
    assert_eq!(
        checkpoint["metadata_revision"], before.revision,
        "guard untouched"
    );
    assert_eq!(
        fixture.events("sync-scope-meta").await?.len(),
        0,
        "nothing appended"
    );
    Ok(())
}

/// The F04 deletion interleave on the synchronous path: a stale Creation
/// proposal must not recreate a child deleted after a sibling pinned the
/// existing revision against the same sync-shaped guard checkpoint.
#[tokio::test]
async fn nats_sync_stale_creation_proposal_cannot_recreate_deleted_child() -> Result<()> {
    let fixture = fixture().await?;
    let (created, request) = fixture
        .seeded_patch_request("srace-meta", "srace-call")
        .await?;

    // Sibling B observed the existing record and pinned Revision(rev) as the
    // invocation's first writer, then applied the patch.
    let b_context = fixture.unsaved_context(&request).await?;
    let plan =
        child_metadata::prepare_sync(&fixture.toolset, &b_context, Some("srace-meta"), patch())
            .await?;
    assert!(matches!(plan, MetadataPlan::Settled), "sibling B applied");
    let pinned = fixture.record("srace-meta").await?.expect("record exists");
    assert!(pinned.revision > created.revision);
    let checkpoint = fixture.checkpoint(&request).await?;
    assert_eq!(checkpoint["metadata_revision"], created.revision);
    assert!(checkpoint.get("prompt_id").is_none(), "sync guard shape");

    // Authorized deletion removes the child's record and stream; the
    // Revision pin survives in the parent invocation's journal row.
    crate::nats_admin::delete_session_state(
        &fixture.js,
        None,
        &fixture.storage("srace-meta"),
        crate::nats_admin::StreamDeletion::First,
    )
    .await?;
    assert!(
        fixture.record("srace-meta").await?.is_none(),
        "record deleted"
    );
    let checkpoint = fixture.checkpoint(&request).await?;
    assert_eq!(
        checkpoint["metadata_revision"], created.revision,
        "the Revision pin survives the deletion"
    );

    // Sibling A paused before pinning with the stale missing-record
    // observation: its Creation proposal is rejected against the Revision
    // winner, recreating nothing.
    let a_context = fixture.unsaved_context(&request).await?;
    let error =
        child_metadata::prepare_sync(&fixture.toolset, &a_context, Some("srace-meta"), patch())
            .await
            .map(|_| ())
            .expect_err("stale Creation proposal must be rejected");
    assert!(
        error
            .to_string()
            .contains("sub-agent metadata checkpoint guard disagrees with the pinned kind"),
        "got: {error}"
    );
    assert!(
        fixture.record("srace-meta").await?.is_none(),
        "the deleted child was not recreated"
    );
    let checkpoint = fixture.checkpoint(&request).await?;
    assert!(
        checkpoint.get("metadata_creation").is_none(),
        "no creation guard was pinned over the Revision winner"
    );

    // The same stale view through the real synchronous entry point also
    // recreates nothing and dispatches nothing.
    let Err(error) =
        termination::run_prompt(&fixture.toolset, sync_patch_params("srace-meta", a_context)).await
    else {
        bail!("stale Creation dispatch must be rejected");
    };
    assert!(
        error.to_string().contains("disagrees with the pinned kind"),
        "got: {error}"
    );
    assert!(
        fixture.record("srace-meta").await?.is_none(),
        "no recreation through run_prompt"
    );
    Ok(())
}

/// A busy-joined prompt's committed user row makes its replay durable: the
/// retry skips fresh composition even though the original attachment is
/// gone, and never resets a later child edit. The busy-join binding exists
/// without a candidate admission record, so the committed row must be
/// checked on that path too.
#[tokio::test]
async fn nats_busy_join_replay_after_attachment_loss_returns_original_response() -> Result<()> {
    let fixture = fixture().await?;
    let session_id = "busyreplay-meta";
    fixture
        .toolset
        .create_session(Some(session_id.into()), None, None)
        .await?;

    let gone_attachment =
        "cid:media:parent/source/cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    let original = MessageContent::Array(vec![
        MessageContentPart::Text {
            text: "original prompt".into(),
        },
        MessageContentPart::ImageUrl {
            image_url: harnx_core::message::ImageUrl {
                url: gone_attachment.into(),
            },
        },
    ]);
    // The busy join bound the prompt to the running invocation and its user
    // row committed; no admission record exists for this call.
    // qual:allow(dry, fragment) reason: "The two sync-replay tests mirror each other by design (admission-bound vs busy-join binding) and share the bind-edit-retry tail; they diverge in what each seeds before it."
    fixture
        .commit_replayed_turn(session_id, "join-call", original.clone())
        .await?;

    // The retry carries the same call id, the gone attachment and the same
    // patch. Composing fresh would fail on the missing attachment; the
    // busy-join replay must not try.
    let (edited, args) = fixture
        .edited_child_retry_args(session_id, gone_attachment)
        .await?;
    fixture
        .assert_sync_replay_never_resets(args, "join-call", session_id, edited.revision)
        .await?;
    Ok(())
}

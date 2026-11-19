//! Session deletion must purge the no-TTL interaction checkpoint bucket: a
//! deleted session cannot leave private result snapshots or tool args behind,
//! while other sessions' records survive the purge.
use super::*;
use crate::interaction_checkpoint::{
    records::{self, AttemptStart, DispatchIntent, DispatchKind},
    round_start_key, round_started,
    store::CheckpointStore,
    RoundStart, ROUND_START_BUCKET,
};
use crate::nats_worker::tests::{spawn_test_nats, TestNatsServer};
use anyhow::{Context, Result};
use harnx_core::tool::ToolCall;
use serde_json::json;

/// Seed what a live middleware round leaves in the bucket for an unfinished
/// call: the round-start ownership proof, one attempt with a dispatch intent,
/// and a chunked private result snapshot with its retained-byte reservation.
/// Suspended, failed and cancelled rounds all persist records with exactly
/// this shape under the session prefix.
// qual:test_helper
async fn seed_unfinished_round(
    store: &CheckpointStore,
    session: &str,
    round: u64,
    call: &str,
) -> Result<()> {
    store
        .start_round(&RoundStart {
            version: 1,
            session_storage_key: session.into(),
            tool_round_seq: round,
            fence_token: 1,
        })
        .await?;
    let prefix = records::call_prefix(session, round, call);
    store
        .put(
            &prefix,
            "attempts.a1.start",
            &AttemptStart {
                version: 1,
                session_storage_key: session.into(),
                tool_round_seq: round,
                tool_call_id: call.into(),
                attempt_id: "a1".into(),
                chain_version: "chain".into(),
                checkpoint: None,
                fence_token: 1,
            },
        )
        .await?;
    store
        .put(
            &prefix,
            "attempts.a1.dispatches.p1.intent",
            &DispatchIntent {
                version: 1,
                attempt_id: "a1".into(),
                physical_invocation_id: "p1".into(),
                kind: DispatchKind::CallTool,
                step_path: vec![],
                call: ToolCall::new(
                    "bash".into(),
                    json!({"command": "true"}),
                    Some(call.into()),
                    None,
                ),
                fence_token: 1,
            },
        )
        .await?;
    // Large enough for the chunked path, so the purge must also clear chunk
    // keys and the per-call retained-byte reservation.
    store
        .put(
            &prefix,
            "attempts.a1.dispatches.p1.snapshot",
            &json!({"result": "x".repeat(300_000)}),
        )
        .await?;
    Ok(())
}

/// Seed `doomed` with one unfinished round per `(round, end_state)` pair and
/// `survivor` with one, on a broker the test must keep alive until the end:
/// `TestNatsServer` kills it and the `TempDir` deletes its store directory
/// when either is dropped.
// qual:test_helper
async fn seeded_sessions(
    doomed: &str,
    survivor: &str,
    doomed_rounds: &[(u64, &str)],
    survivor_round: u64,
    survivor_call: &str,
) -> Result<
    Option<(
        async_nats::jetstream::Context,
        CheckpointStore,
        TestNatsServer,
        tempfile::TempDir,
    )>,
> {
    let Some((url, server, directory)) = spawn_test_nats().await else {
        return Ok(None);
    };
    let js = async_nats::jetstream::new(async_nats::connect(url).await?);
    let store = CheckpointStore::ensure(&js, 1).await?;
    for (round, call) in doomed_rounds {
        seed_unfinished_round(&store, doomed, *round, call).await?;
    }
    seed_unfinished_round(&store, survivor, survivor_round, survivor_call).await?;
    Ok(Some((js, store, server, directory)))
}

// qual:test_helper
async fn session_keys(store: &CheckpointStore, session: &str) -> Result<Vec<String>> {
    store
        .keys(&format!("{}>", records::session_prefix(session)))
        .await
}

#[tokio::test]
async fn session_delete_purges_that_sessions_checkpoint_records_only() -> Result<()> {
    // One round per end state a deleted session can leave behind: suspended,
    // failed and cancelled.
    let Some((js, store, _server, _directory)) = seeded_sessions(
        "oracle/doomed",
        "oracle/survivor",
        &[(3, "suspended"), (4, "failed"), (5, "cancelled")],
        1,
        "suspended",
    )
    .await?
    else {
        return Ok(());
    };

    delete_session_state(&js, None, "oracle/doomed", StreamDeletion::First).await?;

    let leftover = session_keys(&store, "oracle/doomed").await?;
    assert!(
        leftover.is_empty(),
        "deleted session keeps checkpoint records: {leftover:?}"
    );
    assert!(!round_started(&js, "oracle/doomed", 3).await?);
    let survivor_keys = session_keys(&store, "oracle/survivor").await?;
    assert!(
        !survivor_keys.is_empty(),
        "another session's checkpoint records must survive the deletion"
    );
    assert!(round_started(&js, "oracle/survivor", 1).await?);
    Ok(())
}

#[tokio::test]
async fn session_delete_succeeds_without_checkpoint_bucket_and_creates_none() -> Result<()> {
    let Some((url, _server, _directory)) = spawn_test_nats().await else {
        return Ok(());
    };
    let js = async_nats::jetstream::new(async_nats::connect(url).await?);
    // Fresh server, so the bucket doesn't exist and nothing created it.
    delete_session_state(&js, None, "oracle/never-existed", StreamDeletion::First)
        .await
        .context("deletion must tolerate an absent checkpoint bucket")?;
    let error = js
        .get_stream(format!("KV_{ROUND_START_BUCKET}"))
        .await
        .expect_err("deletion must not create the checkpoint bucket");
    assert!(
        stream_missing(&error.kind()),
        "unexpected error creating/finding the bucket: {error}"
    );
    Ok(())
}

#[tokio::test]
async fn session_delete_completes_and_retries_after_partial_checkpoint_purge() -> Result<()> {
    let Some((js, store, _server, _directory)) = seeded_sessions(
        "oracle/half-purged",
        "oracle/survivor",
        &[(3, "suspended"), (4, "cancelled")],
        1,
        "failed",
    )
    .await?
    else {
        return Ok(());
    };
    // A purge that failed part way leaves some keys already gone; the retry
    // must tolerate them and finish the rest.
    js.get_key_value(ROUND_START_BUCKET)
        .await
        .context("open checkpoint bucket for the partial purge")?
        .purge(&round_start_key("oracle/half-purged", 3))
        .await
        .context("simulate the already-purged key")?;

    delete_session_state(&js, None, "oracle/half-purged", StreamDeletion::First).await?;

    let leftover = session_keys(&store, "oracle/half-purged").await?;
    assert!(
        leftover.is_empty(),
        "retry left records behind: {leftover:?}"
    );
    assert!(!session_keys(&store, "oracle/survivor").await?.is_empty());
    // A fully purged deletion stays a succeeding no-op.
    delete_session_state(&js, None, "oracle/half-purged", StreamDeletion::First).await?;
    assert!(session_keys(&store, "oracle/half-purged").await?.is_empty());
    Ok(())
}

use super::fixture::*;
use super::helpers::*;
use super::stub::*;

use anyhow::Result;

/// C8: an unreadable lease state fails closed — the client never takes the
/// unfenced idle fallback while a lease may be held, and errors explicitly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_lease_lookup_error_fails_closed() -> Result<()> {
    reset_model_state();
    let fixture = SurfaceFixture::start("single").await?;
    run_single_turn(&fixture, "prepare the deployment").await?;
    let (surface_id, publish_seq) = fixture.wait_surface_published().await?;

    // Corrupt the session's lease record: the control listener is gone
    // (idle session), so the client can only classify the lease by reading
    // it — and must refuse to write unfenced when it cannot.
    let lease_config = harnx_runtime::nats_lease::NatsLeaseConfig::default();
    let bucket = fixture
        .jetstream
        .get_key_value(&lease_config.bucket)
        .await?;
    bucket
        .put(
            lease_config.key_for_session(fixture.session.storage_key()),
            bytes::Bytes::from_static(b"not a lease record"),
        )
        .await?;

    let entries_before = fixture.entries().await?.len();
    let submission = action_submission(&surface_id, publish_seq, "note_act", None);
    let result = fixture.session.submit_surface_action(&submission).await;
    let error = result.expect_err("unknown lease state must fail closed");
    assert!(
        format!("{error:#}").contains("lease state"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fixture.entries().await?.len(),
        entries_before,
        "no unfenced append while the lease state is unknown"
    );

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}

/// P3 (runtime side): a held lease whose holder never answers — the
/// mixed-version old-worker case — never falls back to an unfenced write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_held_lease_without_answer_never_writes_unfenced() -> Result<()> {
    reset_model_state();
    let fixture = SurfaceFixture::start("single").await?;
    run_single_turn(&fixture, "prepare the deployment").await?;
    let (surface_id, publish_seq) = fixture.wait_surface_published().await?;

    // Simulate an old-version worker that holds the lease and drops the
    // unknown control command: a valid lease record, nobody listening.
    let lease_config = harnx_runtime::nats_lease::NatsLeaseConfig::default();
    let bucket = fixture
        .jetstream
        .get_key_value(&lease_config.bucket)
        .await?;
    let holder = harnx_runtime::nats_lease::LeaseRecord {
        execution_id: None,
        worker_id: "old-mixed-version-worker".into(),
        generation: 1,
        acquired_at: "2026-01-01T00:00:00Z".into(),
        holder: None,
    };
    bucket
        .put(
            lease_config.key_for_session(fixture.session.storage_key()),
            bytes::Bytes::from(serde_json::to_vec(&holder).unwrap()),
        )
        .await?;

    let entries_before = fixture.entries().await?.len();
    let submission = action_submission(&surface_id, publish_seq, "note_act", None);
    let result = fixture.session.submit_surface_action(&submission).await;
    let error = result.expect_err("a held lease with no answer must error, not fall back");
    assert!(
        format!("{error:#}").contains("holds the session lease"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        fixture.entries().await?.len(),
        entries_before,
        "no unfenced append while a lease is held"
    );

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}

use super::*;
use anyhow::Context as _;

/// An old, headerless Put with no holder identity is reclaimable past
/// three effective TTLs of the legacy floor. No liveness check applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_old_holderless_activation_is_reclaimed_and_completed_without_parking() -> Result<()> {
    let session_id = "stale-holderless-recovery";
    let Some((
        _server,
        jetstream,
        ProbedWorker {
            calls,
            probe,
            faults: _faults,
            worker,
        },
    )) = busy_fixture(session_id).await?
    else {
        return Ok(());
    };

    // A legacy headerless record without an identity. Neither broker
    // expiry nor a liveness check applies.
    let record = harnx_runtime::nats_lease::LeaseRecord {
        execution_id: None,
        worker_id: "stranded-foreign-worker".into(),
        generation: 1,
        acquired_at: chrono::Utc::now().to_rfc3339(),
        holder: None,
    };
    let lease_config = NatsLeaseConfig::default();
    let bucket = open_lease_bucket(&jetstream, &lease_config)
        .await
        .context("the ready worker must have created the lease bucket")?;
    bucket
        .create(
            &lease_config.key_for_session(&storage_key(session_id)),
            serde_json::to_vec(&record)?.into(),
        )
        .await?;

    // Three effective TTLs at the 30s legacy floor is 90s, and eligibility
    // is strictly greater.
    tokio::time::sleep(Duration::from_secs(92)).await;

    assert_eq!(
        probe.parked_total.load(Ordering::SeqCst),
        0,
        "nothing may park before the activation is published"
    );
    let reclaims_before = harnx_runtime::nats_metrics::snapshot().lease_reclaims;
    publish_activation_for(
        &jetstream,
        session_id,
        "recover a session stranded by a dead foreign worker",
    )
    .await?;

    // The preflight frees the delivery, the claim reclaims the aged lease
    // through the revision CAS, and the turn runs: exactly one turn,
    // nothing parked, the delivery fully settled.
    wait_until(CI_SAFE_TIMEOUT, || calls.load(Ordering::SeqCst) >= 1).await?;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        harnx_runtime::nats_metrics::snapshot().lease_reclaims > reclaims_before,
        "the claim must have gone through the age reclaim, not a free create"
    );
    assert_eq!(
        probe.parked_total.load(Ordering::SeqCst),
        0,
        "an activation the preflight freed must never park a busy waiter"
    );
    let consumer = worker_consumer(&jetstream).await?;
    wait_until_nothing_pending(&consumer).await?;
    drop(worker);
    Ok(())
}

/// A lease key holding a live Put that is not a lease record must settle
/// like a preflight read error — NAK back, park nothing, launch nothing —
/// and never free the key or wait the lease TTL out. Once the corrupt
/// record is purged, the next redelivery claims cleanly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_malformed_lease_record_settles_without_parking_or_launching() -> Result<()> {
    let session_id = "stale-malformed-lease";
    let Some((
        _server,
        jetstream,
        ProbedWorker {
            calls,
            probe,
            faults: _faults,
            worker,
        },
    )) = busy_fixture(session_id).await?
    else {
        return Ok(());
    };

    // Garbage where the session's lease record belongs: a live Put the
    // preflight can read but never decode.
    let lease_config = NatsLeaseConfig::default();
    let bucket = open_lease_bucket(&jetstream, &lease_config)
        .await
        .context("the ready worker must have created the lease bucket")?;
    let key = lease_config.key_for_session(&storage_key(session_id));
    bucket
        .create(&key, b"not a lease record".to_vec().into())
        .await?;

    let consumer = worker_consumer(&jetstream).await?;
    // Sample before publishing: num_redelivered includes messages already in
    // redelivery state, so a post-publish baseline can include this delivery
    // before the error path NAKs it.
    let before_redeliveries = consumer.get_info().await?.num_redelivered;
    publish_activation_for(&jetstream, session_id, "run after the corrupt lease clears").await?;
    // The delayed preflight NAK hands the delivery back; give it at least
    // two cycles so a claim that wrongly parked or launched would show.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a settled delivery must not launch a turn"
    );
    assert_eq!(
        probe.parked_total.load(Ordering::SeqCst),
        0,
        "a malformed lease must not park a busy waiter"
    );
    assert!(
        consumer.get_info().await?.num_redelivered > before_redeliveries,
        "the settled delivery must come back through the broker"
    );

    // Once the corrupt record is purged, the next redelivery claims
    // cleanly: exactly one turn ever runs for this session.
    bucket.purge(&key).await?;
    wait_until(CI_SAFE_TIMEOUT, || calls.load(Ordering::SeqCst) >= 1).await?;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        probe.parked_total.load(Ordering::SeqCst),
        0,
        "the recovery claim must not park either"
    );
    wait_until_nothing_pending(&consumer).await?;
    drop(worker);
    Ok(())
}

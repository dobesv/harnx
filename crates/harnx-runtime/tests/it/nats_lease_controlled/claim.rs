use super::*;
use async_nats::header::NATS_EXPECTED_LAST_SUBJECT_SEQUENCE;
use async_nats::HeaderMap;
use harnx_runtime::nats_lease::{NatsLeaseAcquireParams, NatsSessionLease};
use harnx_runtime::nats_metrics::snapshot;

const CONTENDER: &str = "contender";

fn params<'a>(
    js: &jetstream::Context,
    session: &'a str,
    ttl: Duration,
) -> NatsLeaseAcquireParams<'a> {
    NatsLeaseAcquireParams {
        jetstream: js.clone(),
        session_id: session,
        worker_id: CONTENDER.into(),
        generation: 2,
        config: lease_config(ttl),
        session_metadata: None,
    }
}

#[tokio::test]
async fn delayed_clock_does_not_spend_the_successful_creates_ttl() -> Result<()> {
    let Some(mut broker) = LeaseBroker::start(Intercept::Clock).await? else {
        return Ok(());
    };
    let session = "delay-before-create";
    let ttl = Duration::from_secs(4);
    let key = lease_config(ttl).key_for_session(session);
    let old_rev = seed_put(&broker.bucket, &key, serde_json::to_vec(&foreign_record())?).await?;
    let sample = stale_sample(observe_put(&broker.bucket, &key).await?.entry.created);
    let mut js = broker.js.clone();
    js.set_timeout(Duration::from_secs(20));
    let answer_clock = async {
        let request = broker.next_clock().await?;
        // Spend more than the claimant's entire TTL inside its clock request,
        // *before* any successful put. The old Put has a much longer wire TTL.
        tokio::time::sleep(ttl + Duration::from_millis(100)).await;
        assert_eq!(
            observe_put(&broker.bucket, &key).await?.entry.revision,
            old_rev
        );
        reply(&broker.client, request, clock_answer(sample)?).await
    };
    let before = snapshot().lease_reclaims;
    let (lease, ()) = tokio::try_join!(
        NatsSessionLease::acquire_for_worker_activation(params(&js, session, ttl), None),
        answer_clock,
    )?;
    let lease =
        lease.context("delayed clock must not prevent a fresh lease from being acquired")?;
    assert_eq!(snapshot().lease_reclaims, before + 1);
    assert!(lease.acquisition_revision() > old_rev);
    assert!(
        lease.revalidate_ownership().await?,
        "the successful create must have a fresh confirmation budget"
    );
    assert!(lease.fence_token() > lease.acquisition_revision());
    assert_ttl(&observe_put(&broker.bucket, &key).await?, ttl);
    lease.release().await?;
    Ok(())
}

#[tokio::test]
async fn contended_generic_and_scoped_claims_do_not_sample_the_clock() -> Result<()> {
    let Some(mut broker) = LeaseBroker::start(Intercept::Clock).await? else {
        return Ok(());
    };
    let session = "generic-clock-isolation";
    let scope = "coordinator";
    let config = lease_config(WRITER_TTL);
    let key = config.key_for_session(session);
    let scoped_key = config.key_for_scope(session, scope)?;
    let value = serde_json::to_vec(&foreign_record())?;
    let rev = seed_put(&broker.bucket, &key, value.clone()).await?;
    let scoped_rev = seed_put(&broker.bucket, &scoped_key, value).await?;
    let sample = stale_sample(observe_put(&broker.bucket, &key).await?.entry.created);
    let before = snapshot().lease_reclaims;
    let mut reads = broker
        .client
        .subscribe(format!(
            "$JS.API.STREAM.MSG.GET.{}",
            broker.bucket.stream_name
        ))
        .await?;
    broker.js.query_account().await?;
    let generic_claims = async {
        let p = || params(&broker.js, session, config.ttl);
        assert!(NatsSessionLease::acquire_scoped(p(), scope)
            .await?
            .is_none());
        assert_contention_reads(&mut reads, &broker.bucket, &scoped_key).await?;
        assert!(NatsSessionLease::acquire(p()).await?.is_none());
        assert_contention_reads(&mut reads, &broker.bucket, &key).await
    };
    tokio::select! {
        biased;
        clock = broker.clock.recv() => panic!("generic or scoped contention sampled clock: {clock:?}"),
        result = generic_claims => result?,
    }
    assert_eq!(snapshot().lease_reclaims, before);
    assert_eq!(observe_put(&broker.bucket, &key).await?.entry.revision, rev);
    assert_eq!(
        observe_put(&broker.bucket, &scoped_key)
            .await?
            .entry
            .revision,
        scoped_rev
    );

    let js = broker.js.clone();
    let (lease, ()) = tokio::try_join!(
        NatsSessionLease::acquire_for_worker_activation(params(&js, session, config.ttl), None),
        broker.reply_clock(sample),
    )?;
    let lease = lease.context("worker claim must sample clock and reclaim the old Put")?;
    assert_eq!(snapshot().lease_reclaims, before + 1);
    assert_eq!(
        observe_put(&broker.bucket, &scoped_key)
            .await?
            .entry
            .revision,
        scoped_rev
    );
    lease.release().await?;
    Ok(())
}

async fn assert_contention_reads(
    reads: &mut Subscriber,
    bucket: &kv::Store,
    key: &str,
) -> Result<()> {
    let subject = format!("$KV.{}.{key}", bucket.name);
    // After the marker check, assessment must read the same explicit key.
    for _ in 0..2 {
        let request = next_request(reads).await?;
        let payload: Value = serde_json::from_slice(&request.payload)?;
        assert_eq!(payload["last_by_subj"], subject);
    }
    Ok(())
}

#[tokio::test]
async fn repeated_renewals_refresh_the_authoritative_age() -> Result<()> {
    let Some(mut broker) = LeaseBroker::start(Intercept::Clock).await? else {
        return Ok(());
    };
    let session = "age-from-renewal";
    let holder = NatsSessionLease::acquire(params(&broker.js, session, WRITER_TTL))
        .await?
        .context("the holder must acquire")?;
    holder.stop_renewal_for_test().await;
    let key = holder.key();
    let mut observed = observe_put(&broker.bucket, key).await?.entry;
    let original_created = observed.created;
    let reclaims_before = snapshot().lease_reclaims;
    for _ in 0..3 {
        assert!(holder.revalidate_ownership().await?);
        let renewed = observe_put(&broker.bucket, key).await?;
        assert_ttl(&renewed, WRITER_TTL);
        assert!(renewed.entry.revision > observed.revision);
        assert!(renewed.entry.created > observed.created);
        assert_eq!(renewed.entry.revision, holder.fence_token());
        observed = renewed.entry;

        // Exactly at the latest Put's threshold but past the original
        // Put's. No wall-clock sleep is needed to distinguish the evidence.
        let sample = observed.created + WRITER_TTL * 3;
        assert!(sample > original_created + WRITER_TTL * 3);
        let js = broker.js.clone();
        let (lease, ()) = tokio::try_join!(
            NatsSessionLease::acquire_for_worker_activation(params(&js, session, WRITER_TTL), None),
            broker.reply_clock(sample),
        )?;
        assert!(
            lease.is_none(),
            "age must track the latest committed renewal"
        );
        assert_eq!(
            observe_put(&broker.bucket, key).await?.entry.revision,
            observed.revision
        );
    }
    assert_eq!(snapshot().lease_reclaims, reclaims_before);
    holder.release().await?;
    Ok(())
}

#[tokio::test]
async fn same_holder_renewal_after_age_assessment_defeats_stale_purge() -> Result<()> {
    let session = "age-purge-race";
    let key = lease_config(WRITER_TTL).key_for_session(session);
    let Some(mut broker) = LeaseBroker::start(Intercept::Write(&key)).await? else {
        return Ok(());
    };
    let mut writes = broker.client.subscribe(WRITE).await?;
    broker.js.query_account().await?;
    let payload = serde_json::to_vec(&foreign_record())?;
    let old_rev = commit_holder_put(&broker.js, &payload, 0).await?;
    let observed = observe_put(&broker.bucket, &key).await?.entry;
    assert_eq!(observed.revision, old_rev);
    let js = broker.js.clone();
    let before = snapshot().lease_reclaims;
    let race = async {
        // Let the initial create conflict reach the real broker.
        let create = next_request(&mut writes).await?;
        assert_write(&create, None, 0);
        forward(&broker.client, REAL_WRITE, create).await?;
        broker.reply_clock(stale_sample(observed.created)).await?;

        // The purge on the wire proves the assessment already chose age
        // reclaim. Hold it back until the *same* holder commits a renewal.
        let purge = next_request(&mut writes).await?;
        assert_write(&purge, Some("PURGE"), old_rev);
        let renewed_rev = commit_holder_put(&broker.js, &payload, old_rev).await?;
        assert!(renewed_rev > old_rev);
        forward(&broker.client, REAL_WRITE, purge).await?;
        Ok::<_, anyhow::Error>(renewed_rev)
    };
    let (claim, renewed_rev) = tokio::try_join!(
        NatsSessionLease::acquire_for_worker_activation(params(&js, session, WRITER_TTL), None),
        race,
    )?;
    assert!(claim.is_none(), "the stale purge must lose the broker CAS");
    assert_eq!(snapshot().lease_reclaims, before);
    let surviving = observe_put(&broker.bucket, &key).await?;
    assert_eq!(surviving.entry.revision, renewed_rev);
    assert!(surviving.entry.created > observed.created);
    assert_eq!(surviving.entry.value.as_ref(), payload.as_slice());
    assert_ttl(&surviving, WRITER_TTL);
    Ok(())
}

async fn commit_holder_put(js: &jetstream::Context, payload: &[u8], revision: u64) -> Result<u64> {
    let mut headers = HeaderMap::new();
    headers.insert(NATS_MESSAGE_TTL, WRITER_TTL.as_secs().to_string());
    headers.insert(NATS_EXPECTED_LAST_SUBJECT_SEQUENCE, revision.to_string());
    let ack = js
        .publish_with_headers(REAL_WRITE, headers, payload.to_vec().into())
        .await?
        .await?;
    Ok(ack.sequence)
}

fn assert_write(write: &Message, operation: Option<&str>, expected_revision: u64) {
    let headers = write
        .headers
        .as_ref()
        .expect("lease writes use NATS headers");
    assert_eq!(headers.get("KV-Operation").map(|v| v.as_str()), operation);
    assert_eq!(
        headers
            .get(NATS_EXPECTED_LAST_SUBJECT_SEQUENCE)
            .map(|v| v.as_str()),
        Some(expected_revision.to_string().as_str())
    );
}

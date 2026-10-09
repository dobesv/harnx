use crate::{context_authority::support::*, support::Broker};
use anyhow::Result;
use async_nats::{header::NATS_EXPECTED_LAST_SUBJECT_SEQUENCE, jetstream};
use harnx_runtime::a2a_events;
use serde_json::json;

#[tokio::test]
async fn serialized_authority_ceiling_is_enforced_before_cas_and_tracks_updated_kv_limit(
) -> Result<()> {
    let f = Fixture::start().await?;
    let lease = f.lease("budget").await?;
    let context = f.claim(&f.a, &lease, "claim").await?;
    let seed = active_task();
    let candidate = |padding: usize| {
        let mut active = seed.clone();
        active.snapshot.task.metadata =
            Some(serde_json::from_value(json!({"padding":"x".repeat(padding)})).unwrap());
        active
    };
    let empty =
        f.a.prepare_context_update(STORAGE, &context.version()?, "budget-write", |state| {
            state.active = Some(candidate(0))
        })
        .await?;
    let ceiling = 1_048_576 - 1024;
    let padding = ceiling - serde_json::to_vec(empty.document())?.len();
    let fits =
        f.a.prepare_context_update(STORAGE, &context.version()?, "budget-write", |state| {
            state.active = Some(candidate(padding))
        })
        .await?;
    let bytes = serde_json::to_vec(fits.document())?.len();
    assert_eq!(bytes, ceiling);
    let saved = f.a.commit_context(&fits).await?;
    let oversized =
        f.a.prepare_context_update(STORAGE, &saved.version()?, "too-big", |state| {
            state.active.as_mut().unwrap().snapshot.task.metadata =
                candidate(padding + 2048).snapshot.task.metadata;
        })
        .await?;
    assert!(f
        .a
        .commit_context(&oversized)
        .await
        .unwrap_err()
        .to_string()
        .contains("payload budget"));
    assert_eq!(
        f.a.read_context(STORAGE).await?.unwrap().revision,
        saved.revision
    );
    let mut metadata_stream = f.js.get_stream("KV_harnx_sessions").await?;
    let mut config = metadata_stream.info().await?.config.clone();
    config.max_message_size = 128 * 1024;
    f.js.update_stream(config).await?;
    assert_eq!(f.metadata.a2a_payload_limit().await?, 128 * 1024);
    assert!(f
        .a
        .commit_context(&fits)
        .await
        .unwrap_err()
        .to_string()
        .contains("payload budget"));
    assert_eq!(
        f.a.read_context(STORAGE).await?.unwrap().revision,
        saved.revision
    );
    println!("Task7 authority: successful_cas_bytes={bytes} broker_payload=1048576 header_reserve=1024 lowered_kv_payload=131072 rejected_without_revision_change=true");
    lease.release().await?;
    Ok(())
}

#[tokio::test]
async fn supported_per_subject_discard_new_never_evicts_predecessor_and_purge_retry_succeeds(
) -> Result<()> {
    harnx_core::require_nextest();
    let (_broker, _, client) = Broker::start().await?;
    let js = jetstream::new(client);
    let mut stream = a2a_events::ensure(&js, 1).await?;
    let subject = "a2a.tasks.probe.task";
    let publish = |predecessor: u64| {
        let mut headers = async_nats::HeaderMap::new();
        headers.insert(NATS_EXPECTED_LAST_SUBJECT_SEQUENCE, predecessor.to_string());
        js.publish_with_headers(subject, headers, "payload".into())
    };
    for seq in 0..a2a_events::SUBJECT_CAPACITY as u64 {
        assert_eq!(publish(seq).await?.await?.sequence, seq + 1);
    }
    assert!(publish(128).await?.await.is_err());
    assert_eq!(stream.info().await?.state.messages, 128);
    assert_eq!(
        stream
            .get_last_raw_message_by_subject(subject)
            .await?
            .sequence,
        128
    );
    // Frozen cutoff removes only seq<2. seq=1 would mean FULL PURGE on NATS2.11.6.
    assert_eq!(stream.purge().filter(subject).sequence(2).await?.purged, 1);
    assert_eq!(publish(128).await?.await?.sequence, 129);
    assert!(stream.get_raw_message(128).await.is_ok());
    assert_eq!(stream.info().await?.state.consumer_count, 0);
    Ok(())
}

#[tokio::test]
async fn startup_rejects_unsafe_existing_event_policy_and_invalid_byte_budget() -> Result<()> {
    harnx_core::require_nextest();
    assert!(a2a_events::config(1, 0).is_err());
    assert!(a2a_events::config(1, -1).is_err());
    let safe = a2a_events::config(1, a2a_events::DEFAULT_MAX_BYTES)?;
    a2a_events::validate(&safe, 1)?;
    let mut bad = safe.clone();
    bad.allow_message_ttl = true;
    assert!(a2a_events::validate(&bad, 1).is_err());
    let mut bad = safe.clone();
    bad.max_age = std::time::Duration::from_secs(1);
    assert!(a2a_events::validate(&bad, 1).is_err());
    let mut bad = safe.clone();
    bad.discard_new_per_subject = false;
    assert!(a2a_events::validate(&bad, 1).is_err());
    let mut bad = safe.clone();
    bad.discard = jetstream::stream::DiscardPolicy::Old;
    let (_broker, _, client) = Broker::start().await?;
    let js = jetstream::new(client);
    // A legacy DiscardOld stream can't be silently accepted or rewritten at startup.
    bad.discard_new_per_subject = false;
    js.create_stream(bad).await?;
    assert!(a2a_events::ensure(&js, 1)
        .await
        .unwrap_err()
        .to_string()
        .contains("unsafe task event stream"));
    Ok(())
}

use super::*;
use harnx_runtime::nats_worker::FencedSessionLogSink;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hitl_handoff_stale_worker_loses_decision_and_execution_race() -> Result<()> {
    require_nextest();
    let Some(server) = common::spawn_nats_server().await? else {
        return Ok(());
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let lease_config = NatsLeaseConfig {
        ttl: std::time::Duration::from_secs(1),
        renew_interval: std::time::Duration::from_millis(200),
        replicas: 1,
        tombstone_ttl: std::time::Duration::from_secs(10),
        ..Default::default()
    };
    let stale_lease = acquire(&jetstream, &lease_config, "stale-hitl-worker")
        .await?
        .context("stale worker acquires lease")?;
    let log = NatsSessionLog::new_with_replicas(jetstream.clone(), source_key(), 1);
    let stale_expected = seed_pending(&log, &stale_lease).await?;
    stale_lease.stop_renewal_for_test().await;
    let replacement_lease = replacement(&jetstream, &lease_config).await?;
    assert!(
        stale_lease.is_held(),
        "stale worker still believes it owns expired lease"
    );
    let replacement_sink = sink(&jetstream, &replacement_lease).await?;
    let replacement_entry = decision(&replacement_lease, true);
    assert!(replacement_sink
        .append_hitl_event_cas(&replacement_entry, stale_expected)
        .await?
        .is_some());
    let stale_sink = sink(&jetstream, &stale_lease).await?;
    let stale_entry = decision(&stale_lease, false);
    assert!(
        stale_sink
            .append_hitl_event_cas(&stale_entry, stale_expected)
            .await?
            .is_none(),
        "the stream tail the replacement already moved rejects the old owner"
    );
    assert!(!stale_lease.revalidate_ownership().await?);
    assert!(replacement_lease.revalidate_ownership().await?);
    let entries = log.load_events_async().await?;
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::HitlApprovalDecision { .. }))
            .count(),
        1
    );
    replacement_lease.release().await?;
    Ok(())
}

async fn acquire(
    js: &async_nats::jetstream::Context,
    config: &NatsLeaseConfig,
    worker: &str,
) -> Result<Option<Arc<NatsSessionLease>>> {
    Ok(NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream: js.clone(),
        session_id: &source_key(),
        worker_id: worker.into(),
        generation: 1,
        config: config.clone(),
        session_metadata: None,
    })
    .await?
    .map(Arc::new))
}

async fn replacement(
    js: &async_nats::jetstream::Context,
    config: &NatsLeaseConfig,
) -> Result<Arc<NatsSessionLease>> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(lease) = acquire(js, config, "replacement-hitl-worker").await? {
                return Ok(lease);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .context("replacement worker did not acquire expired lease")?
}

async fn sink(
    js: &async_nats::jetstream::Context,
    lease: &Arc<NatsSessionLease>,
) -> Result<FencedSessionLogSink> {
    Ok(FencedSessionLogSink::new(
        generation::fenced_backend(js, &source_key()).await?,
        lease.clone(),
    ))
}

fn decision(lease: &NatsSessionLease, approved: bool) -> SessionLogEntry {
    SessionLogEntry::HitlApprovalDecision {
        tool_call_id: "handoff-race-call".into(),
        approved,
        note: None,
        fence_token: lease.fence_token(),
    }
}

async fn seed_pending(log: &NatsSessionLog, lease: &NatsSessionLease) -> Result<u64> {
    log.append_event_async(&SessionLogEntry::ToolCalls {
        text: "pending handoff".into(),
        thought: None,
        calls: vec![ToolCall::new(
            "target_session_handoff".into(),
            json!({"session_id": TARGET_SESSION_ID, "prompt": "run once"}),
            Some("handoff-race-call".into()),
            None,
        )],
        timestamp: None,
        fence_token: Some(lease.fence_token()),
    })
    .await?;
    log.append_event_async(&SessionLogEntry::HitlApprovalRequested {
        tool_call_id: "handoff-race-call".into(),
        summary: "Approve handoff".into(),
        fence_token: lease.fence_token(),
    })
    .await
}

//! Default worker rounds must not reread the transcript to check ownership.
use super::*;
use harnx_runtime::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
use harnx_runtime::nats_worker::FencedSessionLogSink;

async fn install_worker_sink(harness: &RoundHarness) -> Result<Arc<NatsSessionLease>> {
    let lease = Arc::new(
        NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: harness.jetstream.clone(),
            session_id: &harness.storage_key,
            worker_id: "no-middleware-worker".into(),
            generation: 1,
            config: NatsLeaseConfig {
                replicas: 1,
                ..Default::default()
            },
            session_metadata: None,
        })
        .await?
        .context("worker acquires session lease")?,
    );
    let sink = FencedSessionLogSink::new(
        generation::fenced_backend(&harness.jetstream, &harness.storage_key).await?,
        lease.clone(),
    );
    // An unfenced backend has no interaction adapter and would bypass the bug.
    assert!(sink.interaction_session().is_some());
    harness.config.write().session.as_mut().unwrap().runtime =
        Some(Arc::new(Arc::new(sink) as Arc<dyn SessionAppendSink>));
    Ok(lease)
}

async fn round_log_requests(harness: &RoundHarness, call_id: &str) -> Result<(usize, usize)> {
    let mut requests = harness.jetstream.client().subscribe("$JS.API.>").await?;
    harness.jetstream.query_account().await?;
    harness
        .run_round(vec![time_call(call_id)], ToolRoundPersistence::AppendCalls)
        .await?;
    harness.jetstream.query_account().await?;
    let stream = harnx_runtime::nats_session_log::stream_name_for_session(&harness.storage_key);
    let mut reads = 0;
    let mut total = 0;
    while let Ok(Some(request)) =
        tokio::time::timeout(Duration::from_millis(100), requests.next()).await
    {
        if request.subject.split('.').any(|token| token == stream) {
            total += 1;
            if request.subject.as_str() == format!("$JS.API.STREAM.MSG.GET.{stream}")
                || request
                    .subject
                    .as_str()
                    .starts_with(&format!("$JS.API.DIRECT.GET.{stream}"))
            {
                reads += 1;
            }
        }
    }
    Ok((reads, total))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_middleware_round_log_reads_do_not_scale_with_history() -> Result<()> {
    harnx_core::require_nextest();
    let Some(harness) = RoundHarness::start().await? else {
        return Ok(());
    };
    let lease = install_worker_sink(&harness).await?;
    assert!(harness
        .config
        .read()
        .resolved_middleware()?
        .entries
        .is_empty());
    let short = round_log_requests(&harness, "short-history").await?;
    for _ in 0..512 {
        harness
            .log()
            .append_event_async(&SessionLogEntry::Clear)
            .await?;
    }
    let long = round_log_requests(&harness, "long-history").await?;
    eprintln!("session-log (message reads, API requests): short={short:?}, long={long:?}");
    for (reads, total) in [short, long] {
        assert!(
            (1..=4).contains(&reads),
            "expected at most four tail reads, got {reads}"
        );
        assert!(
            total <= 16,
            "expected at most sixteen session-log API requests, got {total}"
        );
    }
    assert_eq!(
        short.0, long.0,
        "history length must not change message read cost"
    );
    let entries = harness.log().load_events_async().await?;
    assert!(
        entries.len() >= 516,
        "regression needs a long durable history"
    );
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::ToolResults { .. }))
            .count(),
        2,
        "both tool rounds completed"
    );
    lease.release().await?;
    harness.stop().await;
    Ok(())
}

async fn owned_round_without_middleware(checkpoint_only: bool) -> Result<()> {
    let Some(harness) = RoundHarness::start().await? else {
        return Ok(());
    };
    let lease = install_worker_sink(&harness).await?;
    let call = time_call("owned-round");
    let input = harness.input();
    let seq = harness
        .config
        .write()
        .append_session_tool_calls(
            &input,
            "checking the time",
            None,
            std::slice::from_ref(&call),
        )?
        .context("persisted ToolCalls sequence")?;
    if checkpoint_only {
        use harnx_runtime::interaction_checkpoint::{
            round_start_key, RoundStart, ROUND_START_BUCKET,
        };
        let store = harness
            .jetstream
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: ROUND_START_BUCKET.into(),
                ..Default::default()
            })
            .await?;
        store
            .create(
                round_start_key(&harness.storage_key, seq),
                serde_json::to_vec(&RoundStart {
                    version: 1,
                    session_storage_key: harness.storage_key.clone(),
                    tool_round_seq: seq,
                    fence_token: lease.fence_token(),
                })?
                .into(),
            )
            .await?;
        assert_eq!(harness.log().last_entry_async().await?.unwrap().0, seq);
    } else {
        // Log records still prove ownership even if the checkpoint bucket vanished.
        harness
            .log()
            .append_event_async(&SessionLogEntry::SuspensionReady {
                tool_round_seq: seq,
                pending: vec![],
                fence_token: lease.fence_token(),
            })
            .await?;
    }
    harness
        .run_round(vec![call], ToolRoundPersistence::ExistingCalls { seq })
        .await?;
    let entries = harness.log().load_events_async().await?;
    assert!(
        entries.iter().any(|(_, entry)| matches!(entry,
            SessionLogEntry::ToolCallCompleted { tool_round_seq, tool_call_id, .. }
                if *tool_round_seq == seq && tool_call_id == "owned-round"
        )),
        "ownership must keep per-call durable completion enabled after middleware removal"
    );
    assert_eq!(harness.journaled_round("owned-round").await?, Some(seq));
    lease.release().await?;
    harness.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_middleware_keeps_checkpoint_only_round_ownership() -> Result<()> {
    owned_round_without_middleware(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_middleware_keeps_log_only_round_ownership() -> Result<()> {
    owned_round_without_middleware(false).await
}

#[cfg(feature = "fault-injection")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_middleware_tail_probe_fails_closed_on_store_error() -> Result<()> {
    let Some(harness) = RoundHarness::start().await? else {
        return Ok(());
    };
    let lease = install_worker_sink(&harness).await?;
    harnx_runtime::fault_injection::simulate_store_error(&harness.storage_key, true);
    let error = harness
        .run_round(
            vec![time_call("store-error")],
            ToolRoundPersistence::AppendCalls,
        )
        .await
        .expect_err("store failure must block dispatch even when ToolCalls is the tail");
    harnx_runtime::fault_injection::simulate_store_error(&harness.storage_key, false);
    assert!(error
        .to_string()
        .contains("simulated checkpoint store unreachable"));
    assert_eq!(harness.journaled_round("store-error").await?, None);
    assert!(harness
        .log()
        .load_events_async()
        .await?
        .iter()
        .all(|(_, entry)| !matches!(entry, SessionLogEntry::ToolResults { .. })));
    lease.release().await?;
    harness.stop().await;
    Ok(())
}

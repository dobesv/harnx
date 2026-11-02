//! Size rejections end a delivery, not the session or its next activation.
use super::*;
use async_nats::jetstream::{
    consumer::{Info as ConsumerInfo, PullConsumer},
    stream::Stream,
};

#[path = "activation_size_failure/in_turn.rs"]
mod in_turn;
#[path = "activation_size_failure/pre_turn.rs"]
mod pre_turn;

fn expected_deliveries_are_acked(info: &ConsumerInfo, deliveries: u64) -> bool {
    info.delivered.consumer_sequence >= deliveries && info.num_ack_pending == 0
}

async fn delivery_is_settled(
    stream: &Stream,
    consumer: &PullConsumer,
    deliveries: u64,
) -> Result<bool> {
    let info = consumer.get_info().await?;
    assert!(
        info.delivered.consumer_sequence <= deliveries,
        "unexpected redelivery: {info:?}"
    );
    if !expected_deliveries_are_acked(&info, deliveries) {
        return Ok(false);
    }
    if stream.get_info().await?.state.messages != 0 {
        return Ok(false);
    }
    assert_eq!(
        info.delivered.consumer_sequence, deliveries,
        "size failures must not redeliver the activation"
    );
    Ok(true)
}

async fn wait_for_settlement(
    stream: &Stream,
    consumer: &PullConsumer,
    deliveries: u64,
) -> Result<()> {
    while !delivery_is_settled(stream, consumer, deliveries).await? {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

async fn assert_settled_after(js: &async_nats::jetstream::Context, deliveries: u64) -> Result<()> {
    let stream = js.get_stream("WORK_NOTIFY_local").await?;
    let consumer: async_nats::jetstream::consumer::PullConsumer = stream
        .get_consumer("workers")
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        wait_for_settlement(&stream, &consumer, deliveries),
    )
    .await??;
    Ok(())
}

fn error_messages(entries: &[(u64, SessionLogEntry)]) -> Vec<&str> {
    entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Error { message, .. } => Some(message.as_str()),
            _ => None,
        })
        .collect()
}

async fn activate_prompt(
    js: &async_nats::jetstream::Context,
    log: &NatsSessionLog,
    key: &str,
    id: &str,
) -> Result<()> {
    let seq = crate::worker::append_admitted_fixture_user(log, id, "continue").await?;
    publish_session_activate(
        js,
        "local",
        &SessionActivate::new(key).with_requested_seq(seq),
        1,
    )
    .await?;
    Ok(())
}

async fn in_turn_size_rejection(oversized_diagnostic: bool) -> Result<()> {
    require_nextest();
    let Some(server) = common::spawn_configured_nats_server("max_payload: 16384\n").await? else {
        return Ok(());
    };
    let js = local_test_nats(server.url()).await?;
    let session_id = "in-turn-size";
    let key = storage_key(session_id);
    seed_session_metadata(&js, session_id).await?;
    let log = NatsSessionLog::new_with_replicas(js.clone(), &key, 1);
    let calls = Arc::new(AtomicUsize::new(0));
    let call_fn = in_turn::call_fn(&js, &key, calls.clone(), oversized_diagnostic);
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "size-failure-worker",
        call_fn,
    )
    .await?;
    activate_prompt(&js, &log, &key, "first-user").await?;
    assert_settled_after(&js, 1).await?;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let entries = log.load_events_async().await?;
    in_turn::assert_diagnostic(&entries, oversized_diagnostic);
    wait_for_worker_session_cleanup(&js, session_id).await?;
    if oversized_diagnostic {
        in_turn::set_stream_limit(&js, &key, -1).await?;
    }

    activate_prompt(&js, &log, &key, "next-user").await?;
    assert_settled_after(&js, 2).await?;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let entries = log.load_events_async().await?;
    assert!(entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::Message { role: MessageRole::Assistant, content, .. }
            if content.to_text() == "done"
    )));
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_turn_size_error_terminates_first_delivery_and_allows_next_prompt() -> Result<()> {
    in_turn_size_rejection(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn size_error_terminates_even_when_diagnostic_cannot_fit() -> Result<()> {
    in_turn_size_rejection(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pre_turn_size_error_skips_retry_budget() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let js = local_test_nats(server.url()).await?;
    let session_id = "pre-turn-size";
    let key = storage_key(session_id);
    seed_session_metadata(&js, session_id).await?;
    let log = NatsSessionLog::new_with_replicas(js.clone(), &key, 1);
    let prompt =
        crate::worker::append_admitted_fixture_user(&log, "first-user", "continue").await?;
    pre_turn::seed_journaled_reply(&js, &log, &key, session_id).await?;
    // Recovery runs before the turn body. The stream's stricter ceiling makes
    // its append fail even though the result fits the broker-level budget.
    let stream_name = harnx_runtime::nats_session_log::stream_name_for_session(&key);
    let stream = js.get_stream(&stream_name).await?;
    let original = stream.get_info().await?.config;
    let mut limited = original.clone();
    limited.max_message_size = 2048;
    js.update_stream(limited).await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "pre-turn-size-worker",
        counting_stub_call_fn(calls.clone()),
    )
    .await?;
    publish_session_activate(
        &js,
        "local",
        &SessionActivate::new(&key).with_requested_seq(prompt),
        1,
    )
    .await?;
    assert_settled_after(&js, 1).await?;
    pre_turn::assert_recovery_rejected(&log, &calls).await?;
    wait_for_worker_session_cleanup(&js, session_id).await?;

    // Removing the injected stream limit must not require reviving the old
    // delivery or clearing session state before admitting a new prompt.
    js.update_stream(original).await?;
    activate_prompt(&js, &log, &key, "next-user").await?;
    assert_settled_after(&js, 2).await?;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(error_messages(&log.load_events_async().await?).len(), 1);
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

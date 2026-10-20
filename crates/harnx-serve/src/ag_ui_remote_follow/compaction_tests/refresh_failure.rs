use super::*;
use harnx_runtime::nats_session_log::{stream_name_for_session, NatsSessionLog};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

struct HistoryReadProxy {
    context: harnx_runtime::nats_event_sink::JetstreamContext,
    fail_next: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for HistoryReadProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl HistoryReadProxy {
    async fn start(client: async_nats::Client, storage_key: &str) -> Self {
        let prefix = format!("compaction_test_{}", uuid::Uuid::new_v4().simple());
        let context = async_nats::jetstream::with_prefix(client.clone(), &prefix);
        let mut requests = client.subscribe(format!("{prefix}.>")).await.unwrap();
        client.flush().await.unwrap();
        let fail_next = Arc::new(AtomicBool::new(false));
        let armed = fail_next.clone();
        let history_read = format!("STREAM.INFO.{}", stream_name_for_session(storage_key));
        let task = tokio::spawn(async move {
            while let Some(request) = futures_util::StreamExt::next(&mut requests).await {
                let suffix = request.subject.as_str().strip_prefix(&prefix).unwrap();
                let fail =
                    suffix == format!(".{history_read}") && armed.swap(false, Ordering::SeqCst);
                let payload = response_payload(&client, suffix, request.payload, fail).await;
                client
                    .publish(request.reply.unwrap(), payload)
                    .await
                    .unwrap();
            }
        });
        Self {
            context,
            fail_next,
            task,
        }
    }
}

async fn response_payload(
    client: &async_nats::Client,
    suffix: &str,
    request: bytes::Bytes,
    fail: bool,
) -> bytes::Bytes {
    if fail {
        return serde_json::to_vec(&serde_json::json!({
            "error": { "code": 503, "err_code": 10008, "description": "temporary history read failure" }
        })).unwrap().into();
    }
    client
        .request(format!("$JS.API{suffix}"), request)
        .await
        .unwrap()
        .payload
}

async fn attached_history(
    config: &harnx_runtime::config::Config,
    storage_key: &str,
) -> (SessionEventStream, HistoryReadProxy) {
    let cluster = harnx_runtime::config::LOCAL_CLUSTER_KEY;
    let jetstream = crate::serve_nats_jetstream(config, cluster).await.unwrap();
    let client = crate::serve_nats_client(config, cluster).await.unwrap();
    let log = NatsSessionLog::new(jetstream, storage_key).with_replicas(1);
    for (_, entry) in fixture(3) {
        log.append_event_async(&entry).await.unwrap();
    }
    let proxy = HistoryReadProxy::start(client.clone(), storage_key).await;
    let stream = SessionEventStream::attach(proxy.context.clone(), client, storage_key)
        .await
        .unwrap();
    append_compaction_after_attach(&log).await;
    (stream, proxy)
}

async fn forward_compaction(
    forwarder: &mut AdvisoryForwarder,
    stream: &mut SessionEventStream,
    storage_key: &str,
) {
    assert!(forwarder
        .forward(
            stream,
            Some(AdvisoryEnvelope::new(6, compacted_event())),
            storage_key,
        )
        .await
        .expect("a failed refresh must not end the remote follow"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compaction_read_failure_emits_cached_history_and_recovers() {
    harnx_core::require_nextest();
    let sandbox = crate::test_support::TestConfigSandbox::new();
    if !crate::test_support::ensure_test_nats().await {
        return;
    }
    let storage_key = format!("remote-refresh-failure-{}", uuid::Uuid::new_v4());
    let (mut stream, proxy) = attached_history(&sandbox.config(), &storage_key).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let mut forwarder = AdvisoryForwarder::new(tx);
    proxy.fail_next.store(true, Ordering::SeqCst);
    forward_compaction(&mut forwarder, &mut stream, &storage_key).await;
    assert_cached_history(&mut rx, &stream);
    assert!(
        !proxy.fail_next.load(Ordering::SeqCst),
        "fault must reach the history read"
    );
    assert!(
        stream.refresh_history().await.unwrap(),
        "next poll recovers durable history"
    );
    assert_eq!(stream.last_applied_seq(), 6);
    forward_compaction(&mut forwarder, &mut stream, &storage_key).await;
    assert_refreshed_remote_events(&mut rx);
}

fn assert_cached_history(
    rx: &mut tokio::sync::mpsc::Receiver<QueuedEvent>,
    stream: &SessionEventStream,
) {
    assert_eq!(stream.last_applied_seq(), 3);
    let completed = serde_json::to_value(rx.try_recv().unwrap().event).unwrap();
    assert_eq!(completed["name"], "session_compacting_completed");
    let expected =
        HistorySnapshot::from_entries(stream.history(), &Session::default(), true).unwrap();
    for event in expected.events() {
        assert_eq!(rx.try_recv().unwrap().event, event);
    }
    assert!(
        rx.try_recv().is_err(),
        "no RUN_ERROR after a transient read failure"
    );
}

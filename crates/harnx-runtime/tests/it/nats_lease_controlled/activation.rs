use super::*;
use crate::worker::{counting_stub_call_fn, poll_until, storage_key, wait_until, EnvGuard};
use async_nats::jetstream::consumer::PullConsumer;
use harnx_core::{message::MessageRole, session::SessionLogEntry};
use harnx_metrics::{ACTIVATION_CLAIMS_TOTAL, ACTIVATION_NAKS_TOTAL, NATS_OPERATIONS_TOTAL};
use harnx_runtime::config::{Config, ConfigLock};
use harnx_runtime::nats_metrics::snapshot;
use harnx_runtime::nats_session_log::NatsSessionLog;
use harnx_runtime::nats_session_metadata::{
    SessionInitializer, SessionMetadata, SessionMetadataStore, SessionOverrides,
};
use harnx_runtime::nats_worker::{
    publish_targeted_session_activate, run_worker_daemon, targeted_consumer_name, BusyWaiterProbe,
    LocalWorkerTarget, SessionActivate, WorkerDaemonConfig,
};
use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

const WORKER: &str = "controlled-lease-worker";
const SESSION: &str = "controlled-lease-session";

struct Counters(Vec<(metrics::Key, u64)>);

impl Counters {
    fn take(recorder: &Snapshotter) -> Self {
        // Snapshotter drains counters; all assertions use the same snapshot.
        Self(
            recorder
                .snapshot()
                .into_vec()
                .into_iter()
                .filter_map(|(key, _, _, value)| {
                    let DebugValue::Counter(count) = value else {
                        return None;
                    };
                    Some((key.key().clone(), count))
                })
                .collect(),
        )
    }

    fn count(&self, name: &str, labels: &[(&str, &str)]) -> u64 {
        self.0
            .iter()
            .filter(|(key, _)| {
                key.name() == name
                    && labels
                        .iter()
                        .all(|&(k, v)| key.labels().any(|l| l.key() == k && l.value() == v))
            })
            .map(|(_, count)| count)
            .sum()
    }

    fn assert_naks(&self, reason: Option<&str>) {
        assert_eq!(
            self.count(ACTIVATION_NAKS_TOTAL, &[]),
            u64::from(reason.is_some())
        );
        if let Some(reason) = reason {
            assert_eq!(
                self.count(ACTIVATION_NAKS_TOTAL, &[("reason", reason)]),
                1,
                "expected {reason}"
            );
        }
    }
}

struct Activation {
    _worker: AbortOnDropHandle<Result<()>>,
    _env: [EnvGuard; 2],
    consumer: PullConsumer,
    probe: Arc<BusyWaiterProbe>,
    calls: Arc<AtomicUsize>,
    log: NatsSessionLog,
    prompt: u64,
    key: String,
    metrics: Snapshotter,
}

impl Activation {
    async fn start(broker: &LeaseBroker) -> Result<Self> {
        let recorder = DebuggingRecorder::new();
        let metrics = recorder.snapshotter();
        recorder
            .install()
            .expect("one metrics recorder per nextest process");
        let env = [
            EnvGuard::set("HARNX_NATS_URL", broker._server.url()),
            EnvGuard::set("HARNX_NATS_TOKEN", ""),
        ];
        let probe = Arc::new(BusyWaiterProbe::default());
        let daemon =
            WorkerDaemonConfig::local(WORKER)?.with_busy_waiter_probe_for_test(probe.clone());
        let config = Arc::new(ConfigLock::new(Config::default()));
        let calls = Arc::new(AtomicUsize::new(0));
        let readiness = harnx_healthz::Readiness::default();
        let worker = AbortOnDropHandle::new(tokio::spawn(run_worker_daemon(
            config,
            daemon,
            Some(counting_stub_call_fn(calls.clone())),
            Some(readiness.clone()),
        )));
        wait_until(CI_SAFE_TIMEOUT, || readiness.is_ready()).await?;
        let consumer = broker
            .js
            .get_stream("LOCAL_WORK_NOTIFY_V2")
            .await?
            .get_consumer(&targeted_consumer_name(WORKER)?)
            .await
            .map_err(anyhow::Error::from_boxed)?;
        let store = SessionMetadataStore::ensure(&broker.js, 1).await?;
        store
            .create(&SessionMetadata::new(
                SESSION,
                SessionInitializer::inline("", Default::default(), SessionOverrides::default()),
            ))
            .await?;
        let storage = storage_key(SESSION);
        let log = NatsSessionLog::new_with_replicas(broker.js.clone(), storage.clone(), 1);
        let prompt = crate::worker::append_admitted_fixture_user(
            &log,
            "controlled-prompt",
            "finish the turn",
        )
        .await?;
        let key = NatsLeaseConfig::default().key_for_session(&storage);
        Ok(Self {
            _worker: worker,
            _env: env,
            consumer,
            probe,
            calls,
            log,
            prompt,
            key,
            metrics,
        })
    }

    async fn publish(&self, broker: &LeaseBroker) -> Result<()> {
        publish_targeted_session_activate(
            &broker.js,
            LocalWorkerTarget::new("__local__", WORKER)?,
            &SessionActivate::targeted(storage_key(SESSION), self.prompt, WORKER),
        )
        .await?;
        Ok(())
    }

    async fn assert_busy(&self, outcome: &str) -> Result<Counters> {
        wait_until(CI_SAFE_TIMEOUT, || {
            self.probe.parked.load(Ordering::SeqCst) == 1
        })
        .await?;
        assert_eq!(self.probe.parked_total.load(Ordering::SeqCst), 1);
        let counters = Counters::take(&self.metrics);
        assert_eq!(
            counters.count(ACTIVATION_CLAIMS_TOTAL, &[("outcome", outcome)]),
            1
        );
        counters.assert_naks(None);
        let info = self.consumer.get_info().await?;
        assert_eq!(info.delivered.consumer_sequence, 1);
        assert_eq!(
            info.num_ack_pending, 1,
            "busy delivery stays with its waiter"
        );
        Ok(counters)
    }

    async fn assert_not_claimed(
        &self,
        broker: &LeaseBroker,
        revision: u64,
        counters: &Counters,
    ) -> Result<()> {
        assert_eq!(self.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            counters.count(NATS_OPERATIONS_TOTAL, &[("op", "lease_reclaim")]),
            0
        );
        // This fixture maps only MSG.GET, not DIRECT.GET. Verify the real
        // Put survived even when the test withheld a leader-read answer.
        let entry = broker
            .bucket
            .entry(&self.key)
            .await?
            .context("the Put must survive")?;
        assert_eq!(entry.operation, kv::Operation::Put);
        assert_eq!(entry.revision, revision);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ttl_headed_foreign_lease_is_reclaimed_and_turn_completes_without_parking() -> Result<()> {
    let Some(mut broker) = LeaseBroker::start(Intercept::Clock).await? else {
        return Ok(());
    };
    let activation = Activation::start(&broker).await?;
    let mut record = foreign_record();
    assert!(record
        .holder
        .as_ref()
        .is_some_and(|holder| holder.host.is_some()));
    // A skewed writer's payload clock would make this lease look like it
    // comes from the future. Only the broker's `created` may establish age.
    record.acquired_at = (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339();
    let revision = seed_put(
        &broker.bucket,
        &activation.key,
        serde_json::to_vec(&record)?,
    )
    .await?;
    let observed = observe_put(&broker.bucket, &activation.key).await?;
    assert_eq!(observed.entry.revision, revision);
    assert_ttl(&observed, WRITER_TTL);
    assert_eq!(
        serde_json::from_slice::<LeaseRecord>(&observed.entry.value)?,
        record
    );
    let sample = stale_sample(observed.entry.created);
    let before = snapshot().lease_reclaims;
    activation.publish(&broker).await?;
    // Both stages must ask for time while the original Put still exists.
    for _ in 0..2 {
        let clock = broker.next_clock().await?;
        assert_eq!(
            observe_put(&broker.bucket, &activation.key)
                .await?
                .entry
                .revision,
            revision
        );
        reply(&broker.client, clock, clock_answer(sample)?).await?;
    }
    poll_until(async || {
        let info = activation.consumer.get_info().await?;
        Ok(info.delivered.consumer_sequence >= 1
            && info.num_pending == 0
            && info.num_ack_pending == 0)
    })
    .await?;
    assert_eq!(
        activation
            .consumer
            .get_info()
            .await?
            .delivered
            .consumer_sequence,
        1
    );
    assert_eq!(activation.calls.load(Ordering::SeqCst), 1);
    assert_eq!(activation.probe.parked_total.load(Ordering::SeqCst), 0);
    assert_eq!(snapshot().lease_reclaims, before + 1);
    Counters::take(&activation.metrics).assert_naks(None);

    let entries = activation.log.load_events_async().await?;
    assert!(entries.iter().any(|(_, entry)| matches!(entry,
        SessionLogEntry::Message { role: MessageRole::Assistant, content, .. } if content.to_text() == "done"
    )), "missing durable assistant output: {entries:#?}");
    assert!(
        entries.iter().any(|(_, entry)| matches!(entry,
            SessionLogEntry::TurnEnd { through_seq, .. } if *through_seq >= activation.prompt
        )),
        "missing durable completed turn: {entries:#?}"
    );
    Ok(())
}

async fn unusable_clock_is_busy(
    make_reply: impl FnOnce(OffsetDateTime) -> Result<Value>,
) -> Result<()> {
    let Some(mut broker) = LeaseBroker::start(Intercept::Clock).await? else {
        return Ok(());
    };
    let activation = Activation::start(&broker).await?;
    let revision = seed_put(
        &broker.bucket,
        &activation.key,
        serde_json::to_vec(&foreign_record())?,
    )
    .await?;
    let sample = stale_sample(
        observe_put(&broker.bucket, &activation.key)
            .await?
            .entry
            .created,
    );
    activation.publish(&broker).await?;
    let clock = broker.next_clock().await?;
    reply(&broker.client, clock, make_reply(sample)?).await?;
    let counters = activation.assert_busy("busy_foreign").await?;
    activation
        .assert_not_claimed(&broker, revision, &counters)
        .await
}

#[tokio::test]
async fn missing_clock_timestamp_parks_without_settling() -> Result<()> {
    unusable_clock_is_busy(|_| Ok(json!({"cluster": {"leader": "n1"}}))).await
}

#[tokio::test]
async fn invalid_clock_timestamp_parks_without_settling() -> Result<()> {
    unusable_clock_is_busy(|_| Ok(json!({"ts": "not RFC3339"}))).await
}

#[tokio::test]
async fn non_string_clock_timestamp_parks_without_settling() -> Result<()> {
    unusable_clock_is_busy(|_| Ok(json!({"ts": 12345}))).await
}

#[tokio::test]
async fn leaderless_clock_reply_parks_without_settling() -> Result<()> {
    unusable_clock_is_busy(|sample| {
        let mut answer = clock_answer(sample)?;
        answer["cluster"] = json!({"name": "test-cluster", "replicas": []});
        Ok(answer)
    })
    .await
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    ReadApi,
    ClockApi,
    ClockTimeout,
    UnusableClock,
    MalformedPut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Preflight,
    Claim,
}

async fn stage_fault(stage: Stage, fault: Fault) -> Result<()> {
    let Some(mut broker) = LeaseBroker::start(Intercept::Read).await? else {
        return Ok(());
    };
    let mut reads = broker.client.subscribe(READ).await?;
    broker.js.query_account().await?;
    let activation = Activation::start(&broker).await?;
    let valid = serde_json::to_vec(&foreign_record())?;
    let malformed = b"not a lease record";
    let mut revision = seed_put(
        &broker.bucket,
        &activation.key,
        if stage == Stage::Preflight && matches!(fault, Fault::MalformedPut) {
            malformed.to_vec()
        } else {
            valid
        },
    )
    .await?;
    let entry = broker
        .bucket
        .entry(&activation.key)
        .await?
        .context("seeded active Put")?;
    let sample = stale_sample(entry.created);
    activation.publish(&broker).await?;
    if stage == Stage::Claim {
        forward_read(&broker.client, &mut reads).await?;
        let preflight = broker.next_clock().await?;
        if matches!(fault, Fault::MalformedPut) {
            revision =
                replace_with_malformed(&broker, &activation.key, revision, malformed).await?;
        }
        reply(&broker.client, preflight, clock_answer(sample)?).await?;
        // This read is the initial create conflict's marker check. The
        // *next* read is the claim's fresh authoritative assessment.
        forward_read(&broker.client, &mut reads).await?;
    }
    fail_assessment(&mut broker, &mut reads, fault).await?;

    let counters =
        if stage == Stage::Claim && matches!(fault, Fault::MalformedPut | Fault::UnusableClock) {
            activation.assert_busy("lease_held").await?
        } else {
            // The first delivery must NAK exactly once. Hold its redelivery
            // at the first lease read so no second attempt can affect counters.
            let _redelivery = next_request(&mut reads).await?;
            assert_eq!(
                activation
                    .consumer
                    .get_info()
                    .await?
                    .delivered
                    .consumer_sequence,
                2
            );
            let expected = match stage {
                Stage::Preflight => "preflight_not_ready",
                Stage::Claim => "claim_error",
            };
            let counters = Counters::take(&activation.metrics);
            counters.assert_naks(Some(expected));
            assert_eq!(activation.probe.parked_total.load(Ordering::SeqCst), 0);
            assert_eq!(
                counters.count(
                    ACTIVATION_CLAIMS_TOTAL,
                    &[("outcome", "preflight_not_ready")]
                ),
                u64::from(stage == Stage::Preflight)
            );
            assert_eq!(
                counters.count(ACTIVATION_CLAIMS_TOTAL, &[("outcome", "error")]),
                0,
                "targeted failures must not escape into the generic settlement handler"
            );
            if matches!(fault, Fault::ClockApi) {
                assert_eq!(
                    counters.count(
                        NATS_OPERATIONS_TOTAL,
                        &[("op", "lease_clock"), ("outcome", "error")]
                    ),
                    1
                );
                assert_eq!(
                    counters.count(
                        NATS_OPERATIONS_TOTAL,
                        &[("op", "lease_clock"), ("outcome", "ok")]
                    ),
                    u64::from(stage == Stage::Claim),
                    "claim stage includes successful preflight clock; preflight stage does not"
                );
            }
            if matches!(fault, Fault::ClockTimeout) {
                assert_eq!(
                    counters.count(
                        NATS_OPERATIONS_TOTAL,
                        &[("op", "lease_clock"), ("outcome", "timeout")]
                    ),
                    1
                );
            }
            counters
        };
    activation
        .assert_not_claimed(&broker, revision, &counters)
        .await
}

async fn forward_read(client: &Client, reads: &mut Subscriber) -> Result<()> {
    forward(client, REAL_READ, next_request(reads).await?).await
}

async fn replace_with_malformed(
    broker: &LeaseBroker,
    key: &str,
    revision: u64,
    value: &[u8],
) -> Result<u64> {
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(NATS_MESSAGE_TTL, WRITER_TTL.as_secs().to_string());
    headers.insert(
        async_nats::header::NATS_EXPECTED_LAST_SUBJECT_SEQUENCE,
        revision.to_string(),
    );
    let ack = broker
        .js
        .publish_with_headers(
            format!("$KV.{}.{key}", broker.bucket.name),
            headers,
            value.to_vec().into(),
        )
        .await?
        .await?;
    Ok(ack.sequence)
}

async fn fail_assessment(
    broker: &mut LeaseBroker,
    reads: &mut Subscriber,
    fault: Fault,
) -> Result<()> {
    let read = next_request(reads).await?;
    let api_error = json!({"error": {"code": 503, "err_code": 10008, "description": "test: JetStream temporarily unavailable"}});
    if matches!(fault, Fault::ReadApi) {
        return reply(&broker.client, read, api_error).await;
    }
    forward(&broker.client, REAL_READ, read).await?;
    match fault {
        Fault::ClockApi => {
            let clock = broker.next_clock().await?;
            reply(&broker.client, clock, api_error).await
        }
        Fault::ClockTimeout => {
            // A subscriber exists but the clock request gets no response:
            // async-nats must produce a real JetStream request timeout.
            let _clock = broker.next_clock().await?;
            Ok(())
        }
        Fault::UnusableClock => {
            let clock = broker.next_clock().await?;
            reply(&broker.client, clock, json!({"ts": 12345})).await
        }
        Fault::MalformedPut => Ok(()),
        Fault::ReadApi => unreachable!("handled before forwarding"),
    }
}

#[tokio::test]
async fn preflight_read_api_error_settles_once() -> Result<()> {
    stage_fault(Stage::Preflight, Fault::ReadApi).await
}

#[tokio::test]
async fn claim_read_api_error_settles_once() -> Result<()> {
    stage_fault(Stage::Claim, Fault::ReadApi).await
}

#[tokio::test]
async fn preflight_clock_api_error_settles_once() -> Result<()> {
    stage_fault(Stage::Preflight, Fault::ClockApi).await
}

#[tokio::test]
async fn claim_clock_api_error_settles_once() -> Result<()> {
    stage_fault(Stage::Claim, Fault::ClockApi).await
}

#[tokio::test]
async fn preflight_clock_timeout_settles_once() -> Result<()> {
    stage_fault(Stage::Preflight, Fault::ClockTimeout).await
}

#[tokio::test]
async fn claim_clock_timeout_settles_once() -> Result<()> {
    stage_fault(Stage::Claim, Fault::ClockTimeout).await
}

#[tokio::test]
async fn preflight_malformed_put_settles_once() -> Result<()> {
    stage_fault(Stage::Preflight, Fault::MalformedPut).await
}

#[tokio::test]
async fn claim_malformed_put_stays_held() -> Result<()> {
    stage_fault(Stage::Claim, Fault::MalformedPut).await
}

#[tokio::test]
async fn claim_unusable_clock_stays_held() -> Result<()> {
    stage_fault(Stage::Claim, Fault::UnusableClock).await
}

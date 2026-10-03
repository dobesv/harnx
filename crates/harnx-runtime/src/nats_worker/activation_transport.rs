//! JetStream topology and subjects for cluster-shared and targeted activation.

use super::activation::SessionActivate;
use super::daemon_config::{WorkerActivationMode, WorkerDaemonConfig};
use crate::config::LOCAL_CLUSTER_KEY;
use anyhow::{Context, Result};
use async_nats::header::{HeaderValue, NATS_MESSAGE_ID};
use async_nats::jetstream::{
    self,
    consumer::{pull, DeliverPolicy},
    stream::{Config as StreamConfig, RetentionPolicy, StorageType},
};
use std::time::Duration;

const WORK_NOTIFY_STREAM_PREFIX: &str = "WORK_NOTIFY_";
const WORK_NOTIFY_CONSUMER_NAME: &str = "workers";
pub(super) const WORK_NOTIFY_ACK_WAIT: Duration = Duration::from_secs(30);
const WORK_NOTIFY_INACTIVE_THRESHOLD: Duration = Duration::from_secs(60 * 60);
/// Deliveries after which JetStream stops delivering an activation no worker
/// settles, such as one that crashes every worker that takes it. Workers
/// terminate an activation themselves at
/// [`super::daemon_runtime::MAX_ACTIVATION_DELIVERIES`], below this, and log
/// why; this limit only catches what they never get to. A message that
/// reaches it is never delivered again, even if the limit is raised later, and
/// NATS 2.12.5 and later keep it in the stream.
const WORK_NOTIFY_MAX_DELIVER: i64 = 200;
const _: () = assert!(super::daemon_runtime::MAX_ACTIVATION_DELIVERIES < WORK_NOTIFY_MAX_DELIVER);
/// Deliveries that keep the plain ack wait before the backoff grows. They
/// cover an activation deferred while its session is busy for a few minutes,
/// or retried through the whole failure budget, without changing its timing.
const WORK_NOTIFY_STEADY_DELIVERIES: usize = 30;
const LOCAL_WORK_NOTIFY_STREAM: &str = "LOCAL_WORK_NOTIFY_V2";
const LOCAL_NOTIFY_SUBJECT: &str = "session_scope.__local__.workers.*.sessions.notify";

pub fn notify_subject(cluster: &str) -> String {
    format!("cluster.{cluster}.sessions.notify")
}

pub fn worker_ready_subject(cluster: &str) -> String {
    format!("cluster.{cluster}.worker.ready")
}

/// Validated, borrowed coordinates for one frontend-owned local worker.
#[derive(Clone, Copy, Debug)]
pub struct LocalWorkerTarget<'a> {
    session_scope: &'a str,
    worker_id: &'a str,
}

impl<'a> LocalWorkerTarget<'a> {
    pub fn new(session_scope: &'a str, worker_id: &'a str) -> Result<Self> {
        validate_local_target(session_scope, worker_id)?;
        Ok(Self {
            session_scope,
            worker_id,
        })
    }

    pub fn session_scope(self) -> &'a str {
        self.session_scope
    }

    pub fn worker_id(self) -> &'a str {
        self.worker_id
    }
}

pub fn targeted_notify_subject(target: LocalWorkerTarget<'_>) -> String {
    format!(
        "session_scope.{}.workers.{}.sessions.notify",
        target.session_scope, target.worker_id
    )
}

pub fn targeted_worker_ready_subject(target: LocalWorkerTarget<'_>) -> String {
    format!(
        "session_scope.{}.workers.{}.worker.ready",
        target.session_scope, target.worker_id
    )
}

fn validate_local_target(session_scope: &str, worker_id: &str) -> Result<()> {
    anyhow::ensure!(
        session_scope == LOCAL_CLUSTER_KEY,
        "targeted worker session scope must be {LOCAL_CLUSTER_KEY}"
    );
    validate_worker_id(worker_id)
}

/// Local worker ids are embedded as one NATS subject token.
pub fn validate_worker_id(worker_id: &str) -> Result<()> {
    anyhow::ensure!(!worker_id.is_empty(), "worker id must not be empty");
    anyhow::ensure!(
        worker_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')),
        "worker id '{worker_id}' must be one NATS-safe subject token containing only ASCII letters, digits, '-' or '_'"
    );
    Ok(())
}

pub fn targeted_consumer_name(worker_id: &str) -> Result<String> {
    validate_worker_id(worker_id)?;
    Ok(format!("local-worker-{worker_id}"))
}

fn notify_stream_name(cluster: &str) -> String {
    format!(
        "{WORK_NOTIFY_STREAM_PREFIX}{}",
        sanitize_name_component(cluster)
    )
}

fn shared_notify_consumer_name() -> String {
    WORK_NOTIFY_CONSUMER_NAME.to_string()
}

fn sanitize_name_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

async fn ensure_notify_stream(
    jetstream: &jetstream::Context,
    cluster: &str,
    subject: &str,
    num_replicas: usize,
) -> Result<jetstream::stream::Stream> {
    let name = notify_stream_name(cluster);
    if let Ok(stream) =
        harnx_metrics::time_nats_operation("stream_lookup", jetstream.get_stream(&name)).await
    {
        return Ok(stream);
    }
    match harnx_metrics::time_nats_operation("stream_create", jetstream
        .create_stream(StreamConfig {
            name: name.clone(),
            description: Some("session activation work queue".to_string()),
            subjects: vec![subject.to_string()],
            retention: RetentionPolicy::WorkQueue,
            storage: StorageType::File,
            num_replicas,
            ..Default::default()
        })).await
    {
        Ok(stream) => Ok(stream),
        Err(create_error) => match harnx_metrics::time_nats_operation("stream_lookup", jetstream.get_stream(&name)).await {
            Ok(stream) => Ok(stream),
            Err(get_error) => Err(anyhow::Error::from(create_error).context(format!(
                "Failed to create notify stream '{name}' for cluster '{cluster}' with {num_replicas} replicas; fallback get also failed: {get_error}"
            ))),
        },
    }
}

async fn ensure_local_notify_stream(
    jetstream: &jetstream::Context,
) -> Result<jetstream::stream::Stream> {
    let stream = open_or_create_local_stream(jetstream).await?;
    validate_local_stream(&stream)?;
    Ok(stream)
}

async fn open_or_create_local_stream(
    jetstream: &jetstream::Context,
) -> Result<jetstream::stream::Stream> {
    if let Ok(stream) = harnx_metrics::time_nats_operation(
        "stream_lookup",
        jetstream.get_stream(LOCAL_WORK_NOTIFY_STREAM),
    )
    .await
    {
        return Ok(stream);
    }
    match harnx_metrics::time_nats_operation(
        "stream_create",
        jetstream.create_stream(StreamConfig {
            name: LOCAL_WORK_NOTIFY_STREAM.to_string(),
            description: Some("frontend-targeted local session activations".to_string()),
            subjects: vec![LOCAL_NOTIFY_SUBJECT.to_string()],
            retention: RetentionPolicy::Interest,
            storage: StorageType::File,
            // Frontend-local stream never spans a NATS cluster.
            num_replicas: 1,
            ..Default::default()
        }),
    )
    .await
    {
        Ok(stream) => Ok(stream),
        Err(create_error) => match harnx_metrics::time_nats_operation(
            "stream_lookup",
            jetstream.get_stream(LOCAL_WORK_NOTIFY_STREAM),
        )
        .await
        {
            Ok(stream) => Ok(stream),
            Err(get_error) => Err(anyhow::Error::from(create_error).context(format!(
                "Failed to create local-v2 notify stream; fallback get also failed: {get_error}"
            ))),
        },
    }
}

fn validate_local_stream(stream: &jetstream::stream::Stream) -> Result<()> {
    let configured = &stream.cached_info().config;
    anyhow::ensure!(
        configured.subjects == [LOCAL_NOTIFY_SUBJECT],
        "existing {LOCAL_WORK_NOTIFY_STREAM} stream has incompatible subjects {:?}; expected [{LOCAL_NOTIFY_SUBJECT}]",
        configured.subjects
    );
    anyhow::ensure!(
        configured.retention == RetentionPolicy::Interest,
        "existing {LOCAL_WORK_NOTIFY_STREAM} stream has incompatible retention {:?}; expected Interest",
        configured.retention
    );
    anyhow::ensure!(
        configured.storage == StorageType::File,
        "existing {LOCAL_WORK_NOTIFY_STREAM} stream has incompatible storage {:?}; expected File",
        configured.storage
    );
    Ok(())
}

/// Publish a cluster-shared activation, idempotent via `Nats-Msg-Id`.
pub async fn publish_session_activate(
    jetstream: &jetstream::Context,
    cluster: &str,
    activation: &SessionActivate,
    num_replicas: usize,
) -> Result<u64> {
    let subject = notify_subject(cluster);
    ensure_notify_stream(jetstream, cluster, &subject, num_replicas).await?;
    publish_activation(jetstream, subject, cluster, activation, activation.msg_id()).await
}

/// Publish an activation to one frontend-owned local worker.
pub async fn publish_targeted_session_activate(
    jetstream: &jetstream::Context,
    target: LocalWorkerTarget<'_>,
    activation: &SessionActivate,
) -> Result<u64> {
    anyhow::ensure!(
        activation.target_worker_id.as_deref() == Some(target.worker_id),
        "targeted activation payload does not target worker '{}'",
        target.worker_id
    );
    let requested_seq = activation
        .requested_seq
        .context("targeted activation is missing requested_seq")?;
    let subject = targeted_notify_subject(target);
    ensure_local_notify_stream(jetstream).await?;
    // Each activation attempt must remain deliverable. A previous worker may
    // have acknowledged the same requested sequence and then stalled before
    // completing it; a sequence-only message ID would make JetStream suppress
    // the recovery publish for its duplicate window. The worker's active-map,
    // lease, and requested-sequence coverage checks make repeated deliveries
    // safe.
    let message_id = targeted_activation_message_id(target, activation, requested_seq);
    publish_activation(
        jetstream,
        subject,
        target.session_scope,
        activation,
        message_id,
    )
    .await
}

fn targeted_activation_message_id(
    target: LocalWorkerTarget<'_>,
    activation: &SessionActivate,
    requested_seq: u64,
) -> String {
    format!(
        "{}:{}:{}:{requested_seq}:{}",
        target.session_scope, target.worker_id, activation.session_id, activation.epoch
    )
}

async fn publish_activation(
    jetstream: &jetstream::Context,
    subject: String,
    cluster: &str,
    activation: &SessionActivate,
    message_id: String,
) -> Result<u64> {
    let payload = serde_json::to_vec(activation).context("serialize SessionActivate")?;
    let headers = activation_headers(HeaderValue::from(message_id));
    let started = std::time::Instant::now();
    let ack = harnx_metrics::time_nats_operation("activation_publish", async {
        jetstream
            .publish_with_headers(subject, headers, payload.into())
            .await?
            .await
    })
    .await
    .context("publish/ack SessionActivate")?;
    tracing::info!(event = "activation_published", session_id = %activation.session_id,
        activation_id = %activation.epoch, agent = activation.agent_name.as_deref().unwrap_or("unknown"), cluster = %cluster,
        delivery_attempt = 0, elapsed_ms = started.elapsed().as_millis() as u64,
        reason = "published", "activation published");
    Ok(ack.sequence)
}

pub(super) fn activation_headers(message_id: HeaderValue) -> async_nats::HeaderMap {
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(NATS_MESSAGE_ID, message_id);
    harnx_telemetry::propagate::inject_current_into_nats(&mut headers);
    headers
}

/// The ack waits JetStream gives an activation's successive deliveries, the
/// last one repeating. NATS replaces the consumer's ack wait with the first
/// entry, and it measures a delayed NAK against the entry for the delivery
/// count too, so a worker asking for `delay` on a later delivery waits
/// `entry - ack_wait` longer. The steady entries leave those delays and the
/// progress heartbeat as they were for the deliveries an activation normally
/// uses; one redelivered past them, by workers that keep crashing on it say,
/// comes back less and less often.
fn activation_backoff(ack_wait: Duration) -> Vec<Duration> {
    let mut backoff = vec![ack_wait; WORK_NOTIFY_STEADY_DELIVERIES];
    backoff.extend([2, 4, 10].map(|factor| ack_wait * factor));
    backoff
}

fn activation_consumer_config(name: &str, subject: &str, ack_wait: Duration) -> pull::Config {
    pull::Config {
        durable_name: Some(name.to_string()),
        deliver_policy: DeliverPolicy::All,
        ack_wait,
        filter_subject: subject.to_string(),
        inactive_threshold: WORK_NOTIFY_INACTIVE_THRESHOLD,
        max_deliver: WORK_NOTIFY_MAX_DELIVER,
        backoff: activation_backoff(ack_wait),
        ..Default::default()
    }
}

pub(super) async fn ensure_activation_consumer(
    jetstream: &jetstream::Context,
    daemon: &WorkerDaemonConfig,
) -> Result<jetstream::consumer::Consumer<pull::Config>> {
    let (stream, consumer_name, subject) = consumer_route(jetstream, daemon).await?;
    let config = activation_consumer_config(&consumer_name, &subject, daemon.activation_ack_wait());
    // `create_consumer` also updates a consumer that already exists, which is
    // how one created by an earlier version, without a delivery limit, gets
    // this one. `get_or_create_consumer` would return it as it was. An update
    // replaces the whole configuration, so `config` carries every field.
    let consumer = harnx_metrics::time_nats_operation(
        "consumer_create",
        stream.create_consumer(config.clone()),
    )
    .await
    .with_context(|| format!("create worker consumer '{consumer_name}'"))?;
    if daemon.activation_mode == WorkerActivationMode::WorkerTargeted {
        validate_targeted_consumer(&consumer, &consumer_name, &config)?;
    }
    Ok(consumer)
}

async fn consumer_route(
    jetstream: &jetstream::Context,
    daemon: &WorkerDaemonConfig,
) -> Result<(jetstream::stream::Stream, String, String)> {
    match daemon.activation_mode {
        WorkerActivationMode::ClusterShared => {
            let subject = notify_subject(&daemon.session_scope);
            Ok((
                ensure_notify_stream(
                    jetstream,
                    &daemon.session_scope,
                    &subject,
                    daemon.lease.replicas,
                )
                .await?,
                shared_notify_consumer_name(),
                subject,
            ))
        }
        WorkerActivationMode::WorkerTargeted => {
            let target = LocalWorkerTarget::new(&daemon.session_scope, &daemon.worker_id)?;
            Ok((
                ensure_local_notify_stream(jetstream).await?,
                targeted_consumer_name(&daemon.worker_id)?,
                targeted_notify_subject(target),
            ))
        }
    }
}

fn validate_targeted_consumer(
    consumer: &jetstream::consumer::Consumer<pull::Config>,
    consumer_name: &str,
    expected: &pull::Config,
) -> Result<()> {
    let configured = &consumer.cached_info().config;
    anyhow::ensure!(
        configured.filter_subject == expected.filter_subject,
        "existing targeted consumer '{consumer_name}' has incompatible filter '{}'; expected '{}'",
        configured.filter_subject,
        expected.filter_subject
    );
    anyhow::ensure!(
        configured.ack_wait == expected.ack_wait,
        "existing targeted consumer '{consumer_name}' has incompatible ack wait {:?}; expected {:?}",
        configured.ack_wait,
        expected.ack_wait
    );
    anyhow::ensure!(
        configured.max_deliver == expected.max_deliver && configured.backoff == expected.backoff,
        "existing targeted consumer '{consumer_name}' has incompatible redelivery limit {} with backoff {:?}; expected {} with {:?}",
        configured.max_deliver,
        configured.backoff,
        expected.max_deliver,
        expected.backoff
    );
    anyhow::ensure!(
        configured.inactive_threshold == expected.inactive_threshold,
        "existing targeted consumer '{consumer_name}' has incompatible inactive threshold {:?}; expected {:?}",
        configured.inactive_threshold,
        expected.inactive_threshold
    );
    Ok(())
}

pub(super) fn spawn_readiness_publisher(
    client: async_nats::Client,
    daemon: &WorkerDaemonConfig,
    identity: &crate::worker_identity::WorkerReadiness,
) -> Result<tokio_util::task::AbortOnDropHandle<()>> {
    let subject = match daemon.activation_mode {
        WorkerActivationMode::ClusterShared => worker_ready_subject(&daemon.session_scope),
        WorkerActivationMode::WorkerTargeted => targeted_worker_ready_subject(
            LocalWorkerTarget::new(&daemon.session_scope, &daemon.worker_id)?,
        ),
    };
    let payload = identity.payload()?;
    Ok(tokio_util::task::AbortOnDropHandle::new(tokio::spawn(
        async move {
            loop {
                if let Err(error) = publish_readiness(&client, &subject, &payload).await {
                    log::warn!("failed to publish worker readiness marker: {error:#}");
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        },
    )))
}

async fn publish_readiness(
    client: &async_nats::Client,
    subject: &str,
    payload: &[u8],
) -> Result<()> {
    client
        .publish(subject.to_string(), payload.to_vec().into())
        .await
        .context("publish worker readiness marker")?;
    client
        .flush()
        .await
        .context("flush worker readiness marker")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_worker_ids_are_single_safe_subject_tokens() {
        for valid in ["local-123", "worker_ABC", "a"] {
            validate_worker_id(valid).expect("valid worker id");
        }
        for invalid in ["", ".", "a.b", "*", ">", "worker name", "é"] {
            assert!(
                validate_worker_id(invalid).is_err(),
                "accepted invalid worker id {invalid:?}"
            );
        }
    }

    #[test]
    fn targeted_routes_have_unique_exact_subjects_and_consumers() {
        let first = "local-11111111-1111-1111-1111-111111111111";
        let second = "local-22222222-2222-2222-2222-222222222222";
        let first_target = LocalWorkerTarget::new(LOCAL_CLUSTER_KEY, first).unwrap();
        let second_target = LocalWorkerTarget::new(LOCAL_CLUSTER_KEY, second).unwrap();
        assert_eq!(
            targeted_notify_subject(first_target),
            format!("session_scope.__local__.workers.{first}.sessions.notify")
        );
        assert_eq!(
            targeted_worker_ready_subject(first_target),
            format!("session_scope.__local__.workers.{first}.worker.ready")
        );
        assert_ne!(
            targeted_notify_subject(first_target),
            targeted_notify_subject(second_target)
        );
        assert_ne!(
            targeted_consumer_name(first).unwrap(),
            targeted_consumer_name(second).unwrap()
        );
    }

    #[test]
    fn activation_backoff_keeps_the_ack_wait_then_grows_within_the_delivery_limit() {
        let ack_wait = Duration::from_secs(30);
        let backoff = activation_backoff(ack_wait);
        assert!(backoff[..WORK_NOTIFY_STEADY_DELIVERIES]
            .iter()
            .all(|wait| *wait == ack_wait));
        assert!(backoff.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(backoff.last() > backoff.first());
        // NATS refuses a backoff with more entries than the consumer allows
        // deliveries.
        assert!(i64::try_from(backoff.len()).unwrap() <= WORK_NOTIFY_MAX_DELIVER);
    }

    #[test]
    fn targeted_reactivation_is_not_suppressed_by_the_prior_attempt() {
        let target = LocalWorkerTarget::new(LOCAL_CLUSTER_KEY, "local-worker").unwrap();
        let mut first = SessionActivate::targeted("session", 42, "local-worker");
        first.epoch = "first-attempt".to_string();
        let mut second = SessionActivate::targeted("session", 42, "local-worker");
        second.epoch = "recovery-attempt".to_string();

        assert_ne!(
            targeted_activation_message_id(target, &first, 42),
            targeted_activation_message_id(target, &second, 42),
        );
    }
}

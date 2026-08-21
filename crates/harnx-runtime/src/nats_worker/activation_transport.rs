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
const WORK_NOTIFY_CONSUMER_PREFIX: &str = "worker-";
const WORK_NOTIFY_ACK_WAIT: Duration = Duration::from_secs(30);
const WORK_NOTIFY_INACTIVE_THRESHOLD: Duration = Duration::from_secs(60 * 60);
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

fn durable_consumer_name(worker_id: &str) -> String {
    format!(
        "{WORK_NOTIFY_CONSUMER_PREFIX}{}",
        sanitize_name_component(worker_id)
    )
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
) -> Result<jetstream::stream::Stream> {
    let name = notify_stream_name(cluster);
    if let Ok(stream) = jetstream.get_stream(&name).await {
        return Ok(stream);
    }
    match jetstream
        .create_stream(StreamConfig {
            name: name.clone(),
            description: Some("session activation work queue".to_string()),
            subjects: vec![subject.to_string()],
            retention: RetentionPolicy::WorkQueue,
            storage: StorageType::File,
            ..Default::default()
        })
        .await
    {
        Ok(stream) => Ok(stream),
        Err(_) => jetstream
            .get_stream(&name)
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| format!("Failed to create notify stream for cluster '{cluster}'")),
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
    if let Ok(stream) = jetstream.get_stream(LOCAL_WORK_NOTIFY_STREAM).await {
        return Ok(stream);
    }
    match jetstream
        .create_stream(StreamConfig {
            name: LOCAL_WORK_NOTIFY_STREAM.to_string(),
            description: Some("frontend-targeted local session activations".to_string()),
            subjects: vec![LOCAL_NOTIFY_SUBJECT.to_string()],
            retention: RetentionPolicy::Interest,
            storage: StorageType::File,
            ..Default::default()
        })
        .await
    {
        Ok(stream) => Ok(stream),
        Err(_) => jetstream
            .get_stream(LOCAL_WORK_NOTIFY_STREAM)
            .await
            .context("get concurrently-created local-v2 notify stream"),
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
) -> Result<u64> {
    let subject = notify_subject(cluster);
    ensure_notify_stream(jetstream, cluster, &subject).await?;
    publish_activation(jetstream, subject, activation, activation.msg_id()).await
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
    let message_id = format!(
        "{}:{}:{}:{requested_seq}",
        target.session_scope, target.worker_id, activation.session_id
    );
    publish_activation(jetstream, subject, activation, message_id).await
}

async fn publish_activation(
    jetstream: &jetstream::Context,
    subject: String,
    activation: &SessionActivate,
    message_id: String,
) -> Result<u64> {
    let payload = serde_json::to_vec(activation).context("serialize SessionActivate")?;
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(NATS_MESSAGE_ID, HeaderValue::from(message_id));
    let ack = jetstream
        .publish_with_headers(subject, headers, payload.into())
        .await
        .context("publish SessionActivate")?
        .await
        .context("ack SessionActivate")?;
    Ok(ack.sequence)
}

pub(super) async fn ensure_activation_consumer(
    jetstream: &jetstream::Context,
    daemon: &WorkerDaemonConfig,
) -> Result<jetstream::consumer::Consumer<pull::Config>> {
    let (stream, consumer_name, subject) = consumer_route(jetstream, daemon).await?;
    let consumer = stream
        .get_or_create_consumer(
            &consumer_name,
            pull::Config {
                durable_name: Some(consumer_name.clone()),
                deliver_policy: DeliverPolicy::All,
                ack_wait: WORK_NOTIFY_ACK_WAIT,
                filter_subject: subject.clone(),
                inactive_threshold: WORK_NOTIFY_INACTIVE_THRESHOLD,
                max_deliver: -1,
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("create worker consumer '{consumer_name}'"))?;
    if daemon.activation_mode == WorkerActivationMode::WorkerTargeted {
        validate_targeted_consumer(&consumer, &consumer_name, &subject)?;
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
                ensure_notify_stream(jetstream, &daemon.session_scope, &subject).await?,
                durable_consumer_name(&daemon.worker_id),
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
    subject: &str,
) -> Result<()> {
    let configured = &consumer.cached_info().config;
    anyhow::ensure!(
        configured.filter_subject == subject,
        "existing targeted consumer '{consumer_name}' has incompatible filter '{}'; expected '{subject}'",
        configured.filter_subject
    );
    anyhow::ensure!(
        configured.ack_wait == WORK_NOTIFY_ACK_WAIT,
        "existing targeted consumer '{consumer_name}' has incompatible ack wait {:?}; expected {:?}",
        configured.ack_wait,
        WORK_NOTIFY_ACK_WAIT
    );
    anyhow::ensure!(
        configured.max_deliver == -1,
        "existing targeted consumer '{consumer_name}' has incompatible max deliveries {}; expected unlimited (-1)",
        configured.max_deliver
    );
    anyhow::ensure!(
        configured.inactive_threshold == WORK_NOTIFY_INACTIVE_THRESHOLD,
        "existing targeted consumer '{consumer_name}' has incompatible inactive threshold {:?}; expected {:?}",
        configured.inactive_threshold,
        WORK_NOTIFY_INACTIVE_THRESHOLD
    );
    Ok(())
}

pub(super) fn spawn_readiness_publisher(
    client: async_nats::Client,
    daemon: &WorkerDaemonConfig,
    identity: &crate::worker_identity::WorkerReadiness,
) -> Result<()> {
    let subject = match daemon.activation_mode {
        WorkerActivationMode::ClusterShared => worker_ready_subject(&daemon.session_scope),
        WorkerActivationMode::WorkerTargeted => targeted_worker_ready_subject(
            LocalWorkerTarget::new(&daemon.session_scope, &daemon.worker_id)?,
        ),
    };
    let payload = identity.payload()?;
    tokio::spawn(async move {
        loop {
            if let Err(error) = publish_readiness(&client, &subject, &payload).await {
                log::warn!("failed to publish worker readiness marker: {error:#}");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
    Ok(())
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
}

//! Restricted NATS permission verification for A2A Backend and Worker roles.
//!
//! Validates service permission boundaries using pinned NATS 2.11.6,
//! exercising positive operations (provisioning, scoped leases, reservations,
//! conditional event publishing, leader reads with consumer_count 0, and
//! coordinated session GC/event purge) alongside negative tests for forbidden subjects.

use crate::support::{Broker, DEADLINE};
use a2a_lf::{Message, Part, Role};
use anyhow::{Context, Result};
use async_nats::{
    header::{NATS_EXPECTED_LAST_SUBJECT_SEQUENCE, NATS_MESSAGE_ID, NATS_MESSAGE_TTL},
    jetstream,
};
use chrono::Utc;
use harnx_a2a_server::{
    exports::{AgentCardMeta, Export},
    identity::Principal,
    store::{
        context::{ContextIdentity, ContextSnapshot},
        A2aStore, DedupeKey, FirstMessageReservation, TaskAllocation,
    },
};
use harnx_core::crypto::sha256;
use harnx_runtime::{
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    nats_session_metadata::{SessionMetadataStore, SESSION_METADATA_BUCKET},
};
use std::time::Duration;

pub const TASK_STREAM_NAME: &str = "HARNX_A2A_TASK_EVENTS";

const NATS_CONFIG_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/it/operations/nats-permissions.conf"
);

fn load_permissions_config() -> String {
    std::fs::read_to_string(NATS_CONFIG_PATH)
        .expect("tests/it/operations/nats-permissions.conf must exist and be readable")
}

fn test_export() -> Export {
    Export {
        public_name: "test-agent".into(),
        agent: "test-agent".into(),
        cluster: Some("runner".into()),
        card_meta: AgentCardMeta {
            name: "test-agent".into(),
            description: String::new(),
            version: "1".into(),
            conversation_starters: vec![],
        },
        lookup_keys: vec![],
    }
}

async fn ensure_task_events_stream(
    js: &jetstream::Context,
    replicas: usize,
) -> Result<jetstream::stream::Stream> {
    harnx_runtime::a2a_events::ensure(js, replicas).await
}

#[tokio::test]
async fn pin_nats_server_version_and_capabilities() -> Result<()> {
    harnx_core::require_nextest();
    let config = load_permissions_config();
    let (_broker, _, admin_client) = Broker::start_with_config(&config).await?;

    let info = admin_client.server_info();
    assert_eq!(
        info.version, "2.11.6",
        "nats-server must be pinned to 2.11.6"
    );
    assert_eq!(
        info.max_payload, 1_048_576,
        "standard NATS 1MiB payload ceiling expected"
    );

    // Verify pinned header capabilities in async-nats 0.50.0
    assert_eq!(NATS_MESSAGE_ID.to_string(), "Nats-Msg-Id");
    assert_eq!(
        NATS_EXPECTED_LAST_SUBJECT_SEQUENCE.to_string(),
        "Nats-Expected-Last-Subject-Sequence"
    );
    assert_eq!(NATS_MESSAGE_TTL.to_string(), "Nats-TTL");

    Ok(())
}

#[tokio::test]
async fn restricted_backend_permissions_positive_and_negative() -> Result<()> {
    harnx_core::require_nextest();
    tokio::time::timeout(DEADLINE, backend_permissions_flow())
        .await
        .context("backend permissions deadline")?
}

async fn backend_permissions_flow() -> Result<()> {
    let config = load_permissions_config();
    let (_broker, url, _) = Broker::start_with_config(&config).await?;

    let (errors, mut error_rx) = tokio::sync::mpsc::unbounded_channel();
    let client = async_nats::ConnectOptions::new()
        .user_and_password("a2a_backend".into(), "a2a_backend_test_password".into())
        .event_callback(move |event| {
            let errors = errors.clone();
            async move {
                if let async_nats::Event::ServerError(error) = event {
                    let _ = errors.send(error.to_string());
                }
            }
        })
        .connect(&url)
        .await?;

    let js = jetstream::new(client.clone());

    // 1. Positive: metadata ensure and task event stream provisioning
    let metadata = SessionMetadataStore::ensure(&js, 1).await?;
    let store = A2aStore::new(metadata.clone());
    let mut task_stream = ensure_task_events_stream(&js, 1).await?;

    assert!(
        error_rx.try_recv().is_err(),
        "unexpected permission error during stream/metadata ensure"
    );

    let storage_key = "backend_perm_session";
    let task_id = format!("{storage_key}.task-uuid-1");

    // 2. Positive: Scoped A2A lease acquisition (sessions/{storage}/a2a/lock)
    let lease = NatsSessionLease::acquire_scoped(
        NatsLeaseAcquireParams {
            jetstream: js.clone(),
            session_id: storage_key,
            worker_id: "a2a-backend-replica-1".into(),
            generation: 1,
            config: NatsLeaseConfig {
                ttl: Duration::from_secs(30),
                renew_interval: Duration::from_secs(10),
                replicas: 1,
                ..NatsLeaseConfig::default()
            },
            session_metadata: None,
        },
        "a2a",
    )
    .await?
    .context("scoped lease acquisition")?;

    assert_eq!(lease.key(), format!("sessions/{storage_key}/a2a/lock"));

    // 3. Positive: First-message reservation under a2a/first-messages/{hash}
    let dedupe_key = DedupeKey {
        cluster: "runner".into(),
        export: "test-agent".into(),
        owner: Some("alice".into()),
        message_id: "msg-perm-1".into(),
    };
    let allocation = TaskAllocation {
        local_id: storage_key.into(),
        storage_key: storage_key.into(),
        task_id: task_id.clone(),
        invocation_id: "inv-1".into(),
        prompt_id: "prompt-1".into(),
        closure_id: "closure-1".into(),
        created_at: Utc::now(),
    };
    let mut msg = Message::new(Role::User, vec![Part::text("hello")]);
    msg.message_id = "msg-perm-1".into();
    let reservation = FirstMessageReservation {
        identity: dedupe_key.clone(),
        fingerprint: "fp-1".into(),
        message: msg,
        allocation: allocation.clone(),
    };

    let (winner, is_new) = store
        .reserve_first_message(&reservation, &lease)
        .await
        .context("reserve first message")?;
    assert!(is_new);
    assert_eq!(winner.allocation.task_id, task_id);

    // 4. Positive: Authoritative context claim and recovery registration
    let export = test_export();
    let principal = Principal::User("alice".into());
    store
        .register_recovery(&export, &principal, &allocation, Some(winner))
        .await
        .context("register recovery")?;

    let claim_write = store
        .prepare_context_claim(
            ContextIdentity {
                storage_key,
                local_id: storage_key,
            },
            &lease,
            "backend-claim-op",
        )
        .await
        .context("prepare context claim")?;
    let claimed_context: ContextSnapshot = store
        .commit_context(&claim_write)
        .await
        .context("commit context claim")?;
    assert_eq!(claimed_context.document.epoch, 1);

    // 5. Positive: Conditional task event publish to a2a.tasks.{storage}.{task_id}
    let subject = format!("a2a.tasks.{storage_key}.task-uuid-1");
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(NATS_MESSAGE_ID, "commit-event-1");
    headers.insert(NATS_EXPECTED_LAST_SUBJECT_SEQUENCE, "0");
    let pub_ack = js
        .publish_with_headers(subject.clone(), headers, "{\"event\":\"Working\"}".into())
        .await?
        .await
        .context("task event publish")?;
    assert_eq!(pub_ack.sequence, 1);

    // 6. Invariant check: Reader inspection confirms consumer_count == 0
    let stream_info = task_stream.info().await?;
    assert_eq!(
        stream_info.state.consumer_count, 0,
        "independent leader readers must not accumulate server-side JetStream consumers"
    );

    // 7. Negative: Backend attempting to create work-queue consumers (forbidden for backend)
    assert!(
        error_rx.try_recv().is_err(),
        "no server error before negative consumer-create test"
    );
    client
        .publish(
            "$JS.API.CONSUMER.CREATE.WORK_NOTIFY_runner.workers",
            "{}".into(),
        )
        .await?;
    client.flush().await?;

    let violation = tokio::time::timeout(Duration::from_secs(5), error_rx.recv())
        .await?
        .context("consumer create violation event")?;
    assert!(
        violation.contains("Permissions Violation"),
        "expected Permissions Violation for consumer create, got: {violation}"
    );

    // 8. Negative: Backend attempting to delete a stream
    client
        .publish(
            format!("$JS.API.STREAM.DELETE.{TASK_STREAM_NAME}"),
            "{}".into(),
        )
        .await?;
    client.flush().await?;
    let violation2 = tokio::time::timeout(Duration::from_secs(5), error_rx.recv())
        .await?
        .context("stream delete violation event")?;
    assert!(
        violation2.contains("Permissions Violation"),
        "expected Permissions Violation for stream delete, got: {violation2}"
    );

    // 9. Negative: Backend attempting unapproved subject
    client
        .publish("unapproved.arbitrary.subject", "denied".into())
        .await?;
    client.flush().await?;
    let violation3 = tokio::time::timeout(Duration::from_secs(5), error_rx.recv())
        .await?
        .context("unapproved subject violation event")?;
    assert!(
        violation3.contains("Permissions Violation"),
        "expected Permissions Violation for unapproved subject, got: {violation3}"
    );

    lease.release().await?;
    Ok(())
}

#[tokio::test]
async fn restricted_worker_permissions_positive_and_negative() -> Result<()> {
    harnx_core::require_nextest();
    tokio::time::timeout(DEADLINE, worker_permissions_flow())
        .await
        .context("worker permissions deadline")?
}

async fn worker_permissions_flow() -> Result<()> {
    let config = load_permissions_config();
    let (_broker, url, _) = Broker::start_with_config(&config).await?;

    let (errors, mut error_rx) = tokio::sync::mpsc::unbounded_channel();
    let client = async_nats::ConnectOptions::new()
        .user_and_password("worker".into(), "worker_test_password".into())
        .event_callback(move |event| {
            let errors = errors.clone();
            async move {
                if let async_nats::Event::ServerError(error) = event {
                    let _ = errors.send(error.to_string());
                }
            }
        })
        .connect(&url)
        .await?;

    let js = jetstream::new(client.clone());
    let storage_key = "worker_perm_session";

    // 1. Positive: Worker execution lease acquisition (sessions/{storage}/lock)
    let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream: js.clone(),
        session_id: storage_key,
        worker_id: "worker-instance-1".into(),
        generation: 1,
        config: NatsLeaseConfig {
            ttl: Duration::from_secs(30),
            renew_interval: Duration::from_secs(10),
            replicas: 1,
            ..NatsLeaseConfig::default()
        },
        session_metadata: None,
    })
    .await?
    .context("worker lease acquisition")?;

    assert_eq!(lease.key(), format!("sessions/{storage_key}/lock"));

    // 2. Positive: Publish session transcript event to sessions.{storage}.log
    let transcript_subject = format!("sessions.{storage_key}.log");
    let pub_ack = client.publish(transcript_subject, "entry".into()).await;
    assert!(
        pub_ack.is_ok(),
        "worker must be allowed to publish transcript"
    );
    client.flush().await?;

    assert!(
        error_rx.try_recv().is_err(),
        "unexpected permission error during positive worker operations"
    );

    // 3. Negative: Worker attempting to publish directly to A2A task events (a2a.tasks.>)
    client
        .publish(
            format!("a2a.tasks.{storage_key}.task-uuid-1"),
            "denied".into(),
        )
        .await?;
    client.flush().await?;

    let violation2 = tokio::time::timeout(Duration::from_secs(5), error_rx.recv())
        .await?
        .context("task events publish violation event")?;
    assert!(
        violation2.contains("Permissions Violation"),
        "expected Permissions Violation for task events, got: {violation2}"
    );

    lease.release().await?;
    Ok(())
}

#[tokio::test]
async fn restricted_worker_can_purge_session_resources_and_preserve_sibling() -> Result<()> {
    harnx_core::require_nextest();
    tokio::time::timeout(DEADLINE, gc_and_purge_flow())
        .await
        .context("coordinated GC deadline")?
}

async fn gc_and_purge_flow() -> Result<()> {
    let config = load_permissions_config();
    let (_broker, url, _) = Broker::start_with_config(&config).await?;

    // Backend client for initial provisioning and A2A resource creation
    let backend_client = async_nats::ConnectOptions::new()
        .user_and_password("a2a_backend".into(), "a2a_backend_test_password".into())
        .connect(&url)
        .await?;
    let backend_js = jetstream::new(backend_client.clone());

    // Worker client for running GC / purging
    let worker_client = async_nats::ConnectOptions::new()
        .user_and_password("worker".into(), "worker_test_password".into())
        .connect(&url)
        .await?;
    let worker_js = jetstream::new(worker_client.clone());

    let metadata = SessionMetadataStore::ensure(&backend_js, 1).await?;
    let _store = A2aStore::new(metadata.clone());
    let mut task_stream = ensure_task_events_stream(&backend_js, 1).await?;

    let kv_sessions = backend_js.get_key_value(SESSION_METADATA_BUCKET).await?;
    let kv_leases = backend_js
        .create_key_value(async_nats::jetstream::kv::Config {
            bucket: "harnx_leases".into(),
            history: 1,
            num_replicas: 1,
            ..Default::default()
        })
        .await?;

    // Setup Target Session
    let target = "gc_target_session";
    let target_hash = sha256(target);
    kv_sessions
        .put(format!("sessions/{target}/a2a/context"), "{}".into())
        .await?;
    kv_sessions
        .put(format!("sessions/{target}/a2a/index"), "{}".into())
        .await?;
    kv_sessions
        .put(format!("a2a.registry.{target_hash}"), "{}".into())
        .await?;
    kv_sessions
        .put(format!("a2a/first-messages/{target_hash}"), "{}".into())
        .await?;
    kv_leases
        .put(format!("sessions/{target}/a2a/lock"), "{}".into())
        .await?;
    kv_leases
        .put(format!("sessions/{target}/lock"), "{}".into())
        .await?;

    let mut h1 = async_nats::HeaderMap::new();
    h1.insert(NATS_MESSAGE_ID, "target-event-1");
    h1.insert(NATS_EXPECTED_LAST_SUBJECT_SEQUENCE, "0");
    backend_js
        .publish_with_headers(
            format!("a2a.tasks.{target}.task-1"),
            h1,
            "{\"status\":\"Working\"}".into(),
        )
        .await?
        .await?;

    // Setup Sibling Session to ensure isolation
    let sibling = "gc_sibling_session";
    let sibling_hash = sha256(sibling);
    kv_sessions
        .put(format!("sessions/{sibling}/a2a/context"), "{}".into())
        .await?;
    kv_sessions
        .put(format!("a2a.registry.{sibling_hash}"), "{}".into())
        .await?;
    kv_leases
        .put(format!("sessions/{sibling}/a2a/lock"), "{}".into())
        .await?;

    let mut h2 = async_nats::HeaderMap::new();
    h2.insert(NATS_MESSAGE_ID, "sibling-event-1");
    h2.insert(NATS_EXPECTED_LAST_SUBJECT_SEQUENCE, "0");
    backend_js
        .publish_with_headers(
            format!("a2a.tasks.{sibling}.task-2"),
            h2,
            "{\"status\":\"Working\"}".into(),
        )
        .await?
        .await?;

    // --- Coordinated Garbage Collection Sequence ---
    // 1. Worker / admin purges session metadata prefix
    let purged = metadata.purge_session_prefix(target).await?;
    assert!(purged >= 2, "purged session-prefixed keys");

    // 2. Coordinated purge of global A2A keys associated with target session
    kv_sessions
        .delete(format!("a2a.registry.{target_hash}"))
        .await?;
    kv_sessions
        .delete(format!("a2a/first-messages/{target_hash}"))
        .await?;

    // 3. Purge target leases (both worker execution and A2A scoped lease)
    kv_leases.delete(format!("sessions/{target}/lock")).await?;
    kv_leases
        .delete(format!("sessions/{target}/a2a/lock"))
        .await?;

    // 4. Worker purges task event subject from HARNX_A2A_TASK_EVENTS
    let worker_task_stream = worker_js.get_stream(TASK_STREAM_NAME).await?;
    worker_task_stream
        .purge()
        .filter(format!("a2a.tasks.{target}.>"))
        .await
        .context("worker purges task event stream for target")?;

    // --- Invariant Verification ---
    // Target resources completely eliminated
    assert!(kv_sessions
        .get(format!("sessions/{target}/a2a/context"))
        .await?
        .is_none());
    assert!(kv_sessions
        .get(format!("sessions/{target}/a2a/index"))
        .await?
        .is_none());
    assert!(kv_sessions
        .get(format!("a2a.registry.{target_hash}"))
        .await?
        .is_none());
    assert!(kv_sessions
        .get(format!("a2a/first-messages/{target_hash}"))
        .await?
        .is_none());
    assert!(kv_leases
        .get(format!("sessions/{target}/lock"))
        .await?
        .is_none());
    assert!(kv_leases
        .get(format!("sessions/{target}/a2a/lock"))
        .await?
        .is_none());
    assert!(task_stream
        .get_last_raw_message_by_subject(&format!("a2a.tasks.{target}.task-1"))
        .await
        .is_err());

    // Sibling resources untouched
    assert!(kv_sessions
        .get(format!("sessions/{sibling}/a2a/context"))
        .await?
        .is_some());
    assert!(kv_sessions
        .get(format!("a2a.registry.{sibling_hash}"))
        .await?
        .is_some());
    assert!(kv_leases
        .get(format!("sessions/{sibling}/a2a/lock"))
        .await?
        .is_some());
    assert!(task_stream
        .get_last_raw_message_by_subject(&format!("a2a.tasks.{sibling}.task-2"))
        .await
        .is_ok());

    // Stream consumer count must remain 0
    let info = task_stream.info().await?;
    assert_eq!(info.state.consumer_count, 0);

    Ok(())
}

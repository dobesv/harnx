//! A JetStream follower answers direct gets from whatever it has applied so
//! far, and NATS can place a consumer on any replica, so on the replicated
//! session metadata bucket a read made right after a write can miss it. These
//! tests run against a broker that keeps both kinds of request for the
//! bucket's stream away from it. A direct get reaches a stream that has
//! applied none of the bucket's writes and answers "not found", just as a
//! lagging follower does, and consumer creation reaches nothing at all. Only
//! requests that the stream leader alone answers still reach the bucket.
use crate::common::{self, NatsServerHandle};
use crate::worker::{
    counting_stub_call_fn, local_nats_runtime_config, spawn_worker_daemon_with_call_fn,
    CI_SAFE_TIMEOUT,
};
use anyhow::{ensure, Context, Result};
use async_nats::jetstream::{self, stream};
use chrono::Utc;
use harnx_core::{
    config_data::{RunLimitsConfig, RunLimitsTimeout},
    event::NullSink,
    message::MessageRole,
    require_nextest,
    session::SessionLogEntry,
};
use harnx_runtime::{
    nats_session_log::NatsSessionLog,
    nats_session_metadata::{
        AdmissionAuthority, CallTimeoutOverride, InvocationAdmission, InvocationIdentity,
        RunIdentity, RunLimitsRecord, SessionInitializer, SessionMetadata, SessionMetadataStore,
        SessionOverrides, SESSION_METADATA_BUCKET,
    },
    nats_worker::{publish_session_activate, SessionActivate},
    utils::create_abort_signal,
    NatsSession, NatsSessionConfig, SessionActivationRoute,
};
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Answers the bucket's direct gets in place of the bucket's own stream.
/// Nothing publishes to its subject, so it never holds a key.
const STALE_REPLICA: &str = "STALE_SESSIONS_REPLICA";

fn stale_replica_config() -> String {
    let bucket = format!("KV_{SESSION_METADATA_BUCKET}");
    format!(
        r#"mappings = {{
  "$JS.API.DIRECT.GET.{bucket}": "$JS.API.DIRECT.GET.{STALE_REPLICA}"
  "$JS.API.DIRECT.GET.{bucket}.>": "$JS.API.DIRECT.GET.{STALE_REPLICA}.>"
  "$JS.API.CONSUMER.CREATE.{bucket}": "harnx.test.unanswered"
  "$JS.API.CONSUMER.CREATE.{bucket}.>": "harnx.test.unanswered.>"
}}
"#
    )
}

struct LaggingBroker {
    server: NatsServerHandle,
    jetstream: jetstream::Context,
    store: SessionMetadataStore,
}

/// Start a broker whose replicas all lag the session metadata bucket, and
/// open the bucket on it the way a frontend or worker does.
async fn lagging_broker() -> Result<Option<LaggingBroker>> {
    require_nextest();
    let Some(server) = common::spawn_configured_nats_server(&stale_replica_config()).await? else {
        return Ok(None);
    };
    let jetstream = jetstream::new(async_nats::connect(server.url()).await?);
    jetstream
        .create_stream(stream::Config {
            name: STALE_REPLICA.into(),
            subjects: vec!["harnx.test.stale-replica".into()],
            allow_direct: true,
            storage: stream::StorageType::Memory,
            ..Default::default()
        })
        .await?;
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    assert_replica_reads_miss_writes(&store).await?;
    Ok(Some(LaggingBroker {
        server,
        jetstream,
        store,
    }))
}

/// Without this, a broker that stopped diverting those requests would let
/// every test here pass without a lagging replica in the way.
async fn assert_replica_reads_miss_writes(store: &SessionMetadataStore) -> Result<()> {
    let bucket = store.kv_store();
    bucket.put("stale-replica-probe", "written".into()).await?;
    ensure!(
        bucket.get("stale-replica-probe").await?.is_none(),
        "a direct get saw a write the stale replica never applied"
    );
    ensure!(
        bucket.keys().await.is_err(),
        "a consumer listed the bucket despite the broker diverting it"
    );
    Ok(())
}

fn external_intent(prompt: &str) -> InvocationAdmission {
    let mut intent = InvocationAdmission::new(
        &AdmissionAuthority::External {
            admitted_at: Utc::now(),
        },
        prompt.into(),
        None,
        None,
    );
    intent.prompt_content = Some(harnx_core::message::MessageContent::Text(prompt.into()));
    intent
}

/// A frontend reads back the admission it reserved and bound a moment ago,
/// before it appends the prompt. A read that missed either write failed the
/// prompt with "prompt admission missing after reservation", and a retry of
/// the same admission with "admission create unconfirmed".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_is_read_back_while_replicas_lag() -> Result<()> {
    let Some(broker) = lagging_broker().await? else {
        return Ok(());
    };
    let store = &broker.store;
    let storage = harnx_core::session_identity::session_key(Some("lagging"), "admission");
    let intent = external_intent("first-prompt");

    let reserved = store.reserve_admission(&storage, &intent, &[]).await?;
    store
        .bind_prompt_admission(&storage, "first-prompt", reserved.invocation_id.as_str())
        .await?;

    let admitted = store
        .prompt_admission(&storage, "first-prompt")
        .await?
        .context("prompt admission missing after reservation")?;
    assert_eq!(admitted, reserved);
    assert_eq!(
        store.reserve_admission(&storage, &intent, &[]).await?,
        reserved,
        "a retry finds the reservation it made instead of failing to confirm it"
    );
    assert_eq!(
        store.active_admission(&storage, &[]).await?,
        Some(reserved),
        "steering joins the run this frontend just reserved"
    );
    Ok(())
}

/// A worker freezing an external run creates the run's limits and then
/// records them again as the run's root. The second create is refused, and
/// only a read of the first confirms it, so every external activation
/// depended on reading a write made milliseconds earlier.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_limits_are_confirmed_while_replicas_lag() -> Result<()> {
    let Some(broker) = lagging_broker().await? else {
        return Ok(());
    };
    let store = &broker.store;
    let storage = harnx_core::session_identity::session_key(Some("lagging"), "limits");
    let record = RunLimitsRecord::admit_root(
        RunIdentity::new(),
        InvocationIdentity::new(),
        Utc::now(),
        RunLimitsConfig {
            timeout_secs: RunLimitsTimeout::Finite(NonZeroU64::new(60).unwrap()),
        },
        None,
        CallTimeoutOverride::Omitted,
    )?;

    let frozen = store
        .load_or_create_run_limits(&storage, record.run_id.as_str(), || Ok(record.clone()))
        .await?;
    assert_eq!(frozen, record);
    store.put_run_limits(&storage, &frozen).await?;
    assert_eq!(
        store
            .get_run_limits(&storage, record.run_id.as_str())
            .await?,
        Some(record)
    );
    Ok(())
}

/// Metadata a frontend creates is what the worker it activates checks first.
/// A miss there terminated the activation as metadata-less.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn created_metadata_is_read_back_while_replicas_lag() -> Result<()> {
    let Some(broker) = lagging_broker().await? else {
        return Ok(());
    };
    let metadata = SessionMetadata::new(
        "lagging-metadata",
        SessionInitializer::named("lagging", Default::default()),
    );
    broker.store.create(&metadata).await?;

    let record = broker
        .store
        .get(&metadata.storage_key())
        .await?
        .context("session metadata missing after create")?;
    assert_eq!(record.metadata.session_id, metadata.session_id);
    Ok(())
}

async fn open_session(broker: &LaggingBroker, session_id: &str) -> Result<NatsSession> {
    NatsSession::new(
        NatsSessionConfig {
            cluster: "local".to_string(),
            initializer: SessionInitializer::inline(
                "",
                Default::default(),
                SessionOverrides::default(),
            ),
            session_id: Some(session_id.to_string()),
            activation_route: SessionActivationRoute::ClusterShared,
        },
        async_nats::connect(broker.server.url()).await?,
        broker.jetstream.clone(),
        create_abort_signal(),
    )
    .await
}

/// The frontend half of a turn, with no worker: admit the prompt, read the
/// admission back, append the prompt and publish its activation. This is
/// where sub-agent calls failed on staging.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frontend_appends_an_admitted_prompt_while_replicas_lag() -> Result<()> {
    let Some(broker) = lagging_broker().await? else {
        return Ok(());
    };
    let session = open_session(&broker, "lagging-frontend").await?;
    let config = local_nats_runtime_config(broker.server.url());
    let input = harnx_runtime::config::input::from_str(&config, "lagging prompt", None);

    let appended = session
        .clone()
        .with_external_admission()
        .admit_input(&input, None)
        .await?;

    assert!(
        appended.execution_id().is_some(),
        "the appended prompt names the invocation it was admitted under"
    );
    let entries = NatsSessionLog::new(broker.jetstream.clone(), session.storage_key())
        .load_events_async()
        .await?;
    assert!(entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::Message {
            role: MessageRole::User,
            ..
        }
    )));
    Ok(())
}

/// A whole turn: the worker reads the metadata, admission and limits the
/// frontend wrote just before activating it. Before the worker read them from
/// the leader, it terminated the activation as metadata-less or refused the
/// prompt for having no durable run admission.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_runs_a_turn_admitted_while_replicas_lag() -> Result<()> {
    let Some(broker) = lagging_broker().await? else {
        return Ok(());
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(broker.server.url()),
        "lagging-worker",
        counting_stub_call_fn(Arc::clone(&calls)),
    )
    .await?;
    let session = open_session(&broker, "lagging-turn").await?;

    let outcome = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        session
            .with_external_admission()
            .run_turn("lagging turn", Arc::new(NullSink), None),
    )
    .await??;

    assert_eq!(outcome.error, None);
    assert_eq!(outcome.response.as_deref(), Some("done"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

/// Write what a frontend writes before it activates a worker: the session's
/// metadata, the prompt's admission and the prompt. None of it is read back,
/// so only the worker's own reads meet the lagging replica.
async fn activate_without_reading_back(broker: &LaggingBroker, session_id: &str) -> Result<String> {
    let initializer =
        SessionInitializer::inline("", Default::default(), SessionOverrides::default());
    let storage = initializer.session_key(session_id);
    broker
        .store
        .create(&SessionMetadata::new(session_id, initializer))
        .await?;
    let intent = external_intent("worker-prompt");
    let reserved = broker
        .store
        .reserve_admission(&storage, &intent, &[])
        .await?;
    broker
        .store
        .bind_prompt_admission(&storage, "worker-prompt", reserved.invocation_id.as_str())
        .await?;
    let prompt_seq = NatsSessionLog::new_with_replicas(broker.jetstream.clone(), &storage, 1)
        .append_event_async(&SessionLogEntry::Message {
            id: Some("worker-prompt".into()),
            role: MessageRole::User,
            content: harnx_core::message::MessageContent::Text("worker prompt".into()),
            timestamp: None,
            fence_token: None,
        })
        .await?;
    publish_session_activate(
        &broker.jetstream,
        "local",
        &SessionActivate::new(&storage).with_requested_seq(prompt_seq),
        1,
    )
    .await?;
    Ok(storage)
}

/// The worker reads the metadata, admission and limits its frontend wrote a
/// moment before activating it. A miss terminated the activation as
/// metadata-less, or refused the prompt as having no durable run admission.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_claims_an_activation_admitted_while_replicas_lag() -> Result<()> {
    let Some(broker) = lagging_broker().await? else {
        return Ok(());
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(broker.server.url()),
        "lagging-claim-worker",
        counting_stub_call_fn(Arc::clone(&calls)),
    )
    .await?;
    let storage = activate_without_reading_back(&broker, "lagging-claim").await?;
    let log = NatsSessionLog::new(broker.jetstream.clone(), &storage);

    let ending = tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            if let Some(ending) =
                log.load_events_async()
                    .await?
                    .into_iter()
                    .find_map(|(_, entry)| match entry {
                        SessionLogEntry::TurnEnd { .. } | SessionLogEntry::Error { .. } => {
                            Some(entry)
                        }
                        _ => None,
                    })
            {
                return Ok::<_, anyhow::Error>(ending);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .context("the worker never finished the admitted turn")??;

    assert!(
        matches!(ending, SessionLogEntry::TurnEnd { .. }),
        "the turn failed instead of completing: {ending:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

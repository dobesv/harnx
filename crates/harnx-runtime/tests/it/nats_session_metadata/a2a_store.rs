//! Session-scoped A2A bytes, CAS, and garbage collection.
use crate::common::spawn_nats_server;
use anyhow::Result;
use harnx_core::require_nextest;
use harnx_runtime::nats_session_metadata::SessionMetadataStore;
use harnx_runtime::nats_session_metadata::{
    a2a_message_key, a2a_session_prefix, a2a_task_key, a2a_tasks_prefix,
};

struct ScenarioKeys {
    key: String,
    other: String,
    task: String,
    other_task: String,
    message: String,
}

impl ScenarioKeys {
    fn new(key: &str, other: &str) -> Self {
        Self {
            key: key.to_string(),
            other: other.to_string(),
            task: a2a_task_key(key, "task"),
            other_task: a2a_task_key(other, "task"),
            message: a2a_message_key(key, "message-hash"),
        }
    }
}

fn assert_key_format_and_prefixes(keys: &ScenarioKeys) {
    assert_eq!(keys.task, format!("sessions/{}/a2a/tasks/task", keys.key));
    assert_eq!(
        keys.message,
        format!("sessions/{}/a2a/messages/message-hash", keys.key)
    );
    assert!(keys.task.starts_with(&a2a_tasks_prefix(&keys.key)));
    assert!(keys.message.starts_with(&a2a_session_prefix(&keys.key)));
}

async fn assert_empty_and_insert_records(
    store: &SessionMetadataStore,
    keys: &ScenarioKeys,
) -> Result<u64> {
    assert!(store.get_a2a_task(&keys.task).await?.is_none());
    let revision = store.put_a2a_task(&keys.task, "task-v1".into()).await?;
    store
        .put_a2a_message(&keys.message, "dedupe".into())
        .await?;
    store
        .put_a2a_task(&keys.other_task, "other-agent".into())
        .await?;
    assert!(store
        .put_a2a_task(&keys.task, "overwrite".into())
        .await
        .is_err());
    assert!(store
        .put_a2a_message(&keys.message, "overwrite".into())
        .await
        .is_err());
    Ok(revision)
}

async fn assert_cas_update_and_stale_rejection(
    store: &SessionMetadataStore,
    task_key: &str,
    revision: u64,
) -> Result<u64> {
    let updated = store
        .update_a2a_task(task_key, "task-v2".into(), revision)
        .await?;
    assert!(updated > revision);
    assert!(store
        .update_a2a_task(task_key, "stale".into(), revision)
        .await
        .is_err());
    Ok(updated)
}

async fn assert_records_listing_and_retrieval(
    store: &SessionMetadataStore,
    keys: &ScenarioKeys,
    updated: u64,
) -> Result<()> {
    assert_eq!(
        store.get_a2a_task(&keys.task).await?,
        Some((b"task-v2".to_vec(), updated))
    );
    assert_eq!(
        store.list_a2a_tasks(&keys.key).await?,
        vec![(b"task-v2".to_vec(), updated)]
    );
    assert_eq!(
        store.get_a2a_message(&keys.message).await?.unwrap().0,
        b"dedupe"
    );
    Ok(())
}

async fn assert_purge_and_cross_agent_isolation(
    store: &SessionMetadataStore,
    keys: &ScenarioKeys,
) -> Result<()> {
    assert_eq!(store.purge_session_prefix(&keys.key).await?, 2);
    assert!(store.get_a2a_task(&keys.task).await?.is_none());
    assert!(store.get_a2a_message(&keys.message).await?.is_none());
    assert!(store.list_a2a_tasks(&keys.key).await?.is_empty());
    assert_eq!(
        store.get_a2a_task(&keys.other_task).await?.unwrap().0,
        b"other-agent"
    );
    assert_eq!(store.list_a2a_tasks(&keys.other).await?.len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a2a_store_bytes_cas_prefix_listing_and_gc() -> Result<()> {
    require_nextest();
    let server = spawn_nats_server()
        .await?
        .expect("nats-server required for A2A store coverage");
    let client = async_nats::connect(server.url()).await?;
    let store = SessionMetadataStore::ensure(&async_nats::jetstream::new(client), 1).await?;

    let keys = ScenarioKeys::new(
        &harnx_core::session_identity::session_key(Some("pkg/agent"), "abc123"),
        &harnx_core::session_identity::session_key(Some("pkg/other"), "abc123"),
    );

    assert_key_format_and_prefixes(&keys);
    let revision = assert_empty_and_insert_records(&store, &keys).await?;
    let updated = assert_cas_update_and_stale_rejection(&store, &keys.task, revision).await?;
    assert_records_listing_and_retrieval(&store, &keys, updated).await?;
    assert_purge_and_cross_agent_isolation(&store, &keys).await?;
    Ok(())
}

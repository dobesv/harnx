//! Access rules must authorize before reading or migrating a session's task index.
use super::store_nats::{bind, export, snapshot, start_store};
use anyhow::Result;
use chrono::Utc;
use harnx_a2a_server::{
    identity::Principal,
    store::{parse_task_id, A2aStore, TaskRecord},
};
use harnx_core::access_rules::AccessRules;
use harnx_runtime::nats_session_metadata::{a2a_task_key, SessionMetadataStore};
use std::sync::Arc;

async fn seed_legacy_task(metadata: &SessionMetadataStore, key: &str) -> Result<String> {
    let task = snapshot("access_index");
    let task_id = task.id.clone();
    let (_, uuid) = parse_task_id(&task_id)?;
    let record = TaskRecord {
        version: 1,
        task,
        user_msg_id: "user-msg".into(),
        user_msg_seq: 42,
        execution_id: "execution".into(),
        revision: 1,
        stream_seq: 0,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    metadata
        .put_a2a_task(
            &a2a_task_key(key, uuid),
            serde_json::to_vec(&record)?.into(),
        )
        .await?;
    Ok(task_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_nats_index_authorizes_before_migration_and_keeps_admin_scope() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let rules = Arc::new(AccessRules::from_yaml(
        "rules:\n  - agents: [pkg/agent@test-cluster]\n    users: [alice, bob]\n  - agents: [pkg/agent@test-cluster]\n    users: [admin]\n    scopes: [admin]\n",
    )?);
    let store = A2aStore::new_with_access_rules(metadata.clone(), Some(rules.clone()));
    assert!(std::ptr::eq(store.access_rules().unwrap(), rules.as_ref()));
    let export = export("pkg/agent");
    let local_id = "access_index";
    let key = bind(&metadata, &store, &export, local_id).await?;
    let task_id = seed_legacy_task(&metadata, &key).await?;

    // A denied lookup must not trigger the legacy full-bucket scan or create an index.
    for user in [Principal::User("bob".into()), Principal::Anonymous] {
        assert!(store
            .list_task_index(&export, &user.clone().into(), local_id)
            .await?
            .is_none());
        assert!(store
            .get_task_for_export(&export, &user.clone().into(), &task_id)
            .await?
            .is_none());
        assert!(metadata.get_a2a_task_index(&key).await?.is_none());
    }

    let admin = Principal::User("admin".into());
    let (_, index) = store
        .list_task_index(&export, &admin.clone().into(), local_id)
        .await?
        .unwrap();
    assert_eq!(index.entries.len(), 1);
    assert_eq!(index.entries[0].task_id, task_id);
    assert!(store
        .get_task_for_export(&export, &admin.clone().into(), &task_id)
        .await?
        .is_some());
    assert_eq!(
        store.get_binding(&key).await?.unwrap().owner.as_deref(),
        Some("alice")
    );

    // Index presence cannot turn strict ownership off when no policy is configured.
    let strict = A2aStore::new(metadata);
    assert!(strict.access_rules().is_none());
    assert!(strict
        .list_task_index(&export, &admin.clone().into(), local_id)
        .await?
        .is_none());
    assert!(strict
        .list_task_index(&export, &Principal::User("alice".into()).into(), local_id)
        .await?
        .is_some());
    Ok(())
}

//! Isolated broker tests, following harnx-runtime/tests/it/common/mod.rs.
use crate::support::Broker;
use a2a_lf::{Part, Task, TaskState, TaskStatus};
use anyhow::Result;
use chrono::Utc;
use harnx_a2a_server::{
    exports::{AgentCardMeta, Export},
    identity::Principal,
    store::{
        message_fingerprint, message_id_hash, new_task_id, parse_task_id, A2aBinding, A2aStore,
        ContextAccess, MessageIdentity, StoreError, TaskChanges, TaskRecord, TaskSeed, TaskVersion,
        A2A_BINDING_NAMESPACE,
    },
};
use harnx_core::session_identity::session_key;
use harnx_runtime::nats_session_metadata::{
    a2a_message_key, a2a_task_key, SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use serde_json::json;

pub(super) async fn start_store() -> Result<(Broker, SessionMetadataStore)> {
    harnx_core::require_nextest();
    let (broker, _, client) = Broker::start().await?;
    let store = SessionMetadataStore::ensure(&async_nats::jetstream::new(client), 1).await?;
    Ok((broker, store))
}

pub(super) fn export(agent: &str) -> Export {
    Export {
        public_name: agent.replace('/', "__"),
        agent: agent.into(),
        cluster: Some("test-cluster".into()),
        card_meta: AgentCardMeta {
            name: agent.into(),
            description: String::new(),
            version: "1".into(),
            conversation_starters: vec![],
        },
        lookup_keys: vec![],
    }
}
pub(super) fn alice() -> Principal {
    Principal::User("alice".into())
}

pub(super) async fn list_tasks(
    store: &A2aStore,
    export: &Export,
    owner: &Principal,
    local_id: &str,
) -> Result<Option<Vec<TaskRecord>>> {
    crate::support::list_all_tasks_for_test(store, export, owner, local_id).await
}

pub(super) async fn bind(
    metadata: &SessionMetadataStore,
    store: &A2aStore,
    export: &Export,
    local_id: &str,
) -> Result<String> {
    let key = session_key(Some(&export.agent), local_id);
    metadata
        .create(&SessionMetadata::new(
            local_id,
            SessionInitializer::named(&export.agent, Default::default()),
        ))
        .await?
        .expect("new session");
    let mut record = metadata.get(&key).await?.expect("session metadata");
    let binding = A2aBinding {
        version: 1,
        export: export.public_name.clone(),
        agent: export.agent.clone(),
        cluster: export.cluster.clone().unwrap(),
        owner: Some("alice".into()),
        created_at: Utc::now(),
    };
    store.write_binding(&key, &mut record, &binding).await?;
    Ok(key)
}

pub(super) fn snapshot(local_id: &str) -> Task {
    let mut task: Task = serde_json::from_value(json!({
        "id": new_task_id(local_id), "contextId": local_id,
        "status": {"state": "TASK_STATE_WORKING"},
        "artifacts": [{"artifactId": "answer", "parts": [{"text": "partial answer"}]}],
        "history": [{"role": "ROLE_USER", "messageId": "message/with.dots", "parts": [
            {"text": "Analyze the work item"}, {"data": {"issue": {"fields": {"key": "AW26-11"}}, "invocationType": "ISSUE_ASSIGNMENT"}}
        ]}]
    })).unwrap();
    task.status.timestamp = Some(Utc::now());
    task
}
pub(super) async fn create(store: &A2aStore, key: &str, task: Task) -> Result<TaskRecord> {
    store
        .create_task(
            key,
            TaskSeed {
                task,
                user_msg_id: "user-msg".into(),
                user_msg_seq: 42,
                execution_id: "execution".into(),
            },
        )
        .await
}
pub(super) fn completed() -> TaskStatus {
    TaskStatus {
        state: TaskState::Completed,
        message: None,
        timestamp: Some(Utc::now()),
    }
}

fn assert_round_trip(loaded: &TaskRecord, created: &TaskRecord, task: &Task) -> Result<()> {
    assert_eq!(&loaded.task, task);
    assert_eq!(
        serde_json::to_value(loaded)?,
        serde_json::to_value(created)?
    );
    assert_eq!(loaded.version, 1);
    assert_correlation_fields(loaded);
    Ok(())
}

fn assert_correlation_fields(loaded: &TaskRecord) {
    for (actual, expected) in [
        (loaded.user_msg_id.as_str(), "user-msg"),
        (loaded.execution_id.as_str(), "execution"),
    ] {
        assert_eq!(actual, expected);
    }
    assert_eq!(loaded.user_msg_seq, 42);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_task_record_round_trip() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let export = export("pkg/agent");
    let key = bind(&metadata, &store, &export, "abc123").await?;
    let task = snapshot("abc123");
    let created = create(&store, &key, task.clone()).await?;
    // A fresh wrapper must read durable state, not an in-process cache.
    let restarted = A2aStore::new(metadata.clone());
    let loaded = restarted
        .get_task_for_export(&export, &alice(), &task.id)
        .await?
        .unwrap();
    assert_round_trip(&loaded, &created, &task)?;
    assert!(
        create(&store, &key, task.clone()).await.is_err(),
        "create must not overwrite"
    );
    let (_, uuid) = parse_task_id(&task.id)?;
    assert!(metadata
        .get_a2a_task(&a2a_task_key(&key, uuid))
        .await?
        .is_some());
    assert!(
        metadata
            .get_a2a_task(&a2a_task_key("abc123", uuid))
            .await?
            .is_none(),
        "local ID is not a broker key"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_cas_conflict_on_stale_revision() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let key = bind(&metadata, &store, &export("pkg/agent"), "abc123").await?;
    let record = create(&store, &key, snapshot("abc123")).await?;
    let task_key = a2a_task_key(&key, parse_task_id(&record.task.id)?.1);
    let (bytes, broker_revision) = metadata.get_a2a_task(&task_key).await?.unwrap();
    assert_ne!(
        record.revision, broker_revision,
        "metadata writes advance bucket revision"
    );
    let updated = store
        .update_task(
            TaskVersion {
                storage_key: &key,
                task_id: &record.task.id,
                revision: record.revision,
            },
            TaskChanges {
                status: Some(completed()),
                artifacts: None,
                history: None,
            },
        )
        .await?;
    assert_eq!(updated.revision, record.revision + 1);
    let error = store
        .update_task(
            TaskVersion {
                storage_key: &key,
                task_id: &record.task.id,
                revision: record.revision,
            },
            TaskChanges {
                status: None,
                artifacts: Some(vec![]),
                history: None,
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("CAS conflict"), "{error:#}");
    let error = metadata
        .update_a2a_task(&task_key, bytes.into(), broker_revision)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("CAS update"), "{error:#}");
    assert_eq!(
        store.get_task(&key, &record.task.id).await?.unwrap().task,
        updated.task
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_binding_mismatch_is_not_found() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let export = export("pkg/agent");
    let key = bind(&metadata, &store, &export, "abc123").await?;
    let record = create(&store, &key, snapshot("abc123")).await?;
    let mut other = export.clone();
    other.public_name = "another-export".into();
    assert!(store
        .resolve_context(&other, &alice(), "abc123")
        .await?
        .is_none());
    assert!(store
        .get_task_for_export(&other, &alice(), &record.task.id)
        .await?
        .is_none());
    assert!(list_tasks(&store, &other, &alice(), "abc123")
        .await?
        .is_none());
    other = export.clone();
    other.cluster = Some("other-cluster".into());
    assert!(store
        .resolve_context(&other, &alice(), "abc123")
        .await?
        .is_none());
    assert!(store
        .resolve_context(&export, &alice(), "missing")
        .await?
        .is_none());
    assert!(metadata
        .get(&session_key(Some(&export.agent), "missing"))
        .await?
        .is_none());
    // Non-A2A sessions cannot be resumed either.
    metadata
        .create(&SessionMetadata::new(
            "unbound",
            SessionInitializer::named(&export.agent, Default::default()),
        ))
        .await?;
    assert!(store
        .resolve_context(&export, &alice(), "unbound")
        .await?
        .is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_foreign_owner_is_not_found() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let export = export("pkg/agent");
    let key = bind(&metadata, &store, &export, "abc123").await?;
    let record = create(&store, &key, snapshot("abc123")).await?;
    for principal in [Principal::User("bob".into()), Principal::Anonymous] {
        assert!(store
            .resolve_context(&export, &principal, "abc123")
            .await?
            .is_none());
        assert!(store
            .get_task_for_export(&export, &principal, &record.task.id)
            .await?
            .is_none());
        assert!(list_tasks(&store, &export, &principal, "abc123")
            .await?
            .is_none());
        let error = store
            .dedupe_task(
                ContextAccess {
                    export: &export,
                    owner: &principal,
                    local_id: "abc123",
                },
                MessageIdentity {
                    message_id: "message",
                    fingerprint: "fingerprint",
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<StoreError>(),
            Some(&StoreError::NotFound)
        );
    }
    let missing = store
        .dedupe_task(
            ContextAccess {
                export: &export,
                owner: &alice(),
                local_id: "missing",
            },
            MessageIdentity {
                message_id: "message",
                fingerprint: "fingerprint",
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        missing.downcast_ref::<StoreError>(),
        Some(&StoreError::NotFound)
    );
    Ok(())
}

struct BoundAgent {
    export: Export,
    key: String,
}

struct AgentPair {
    a: BoundAgent,
    b: BoundAgent,
}

async fn bind_agent_pair(metadata: &SessionMetadataStore, store: &A2aStore) -> Result<AgentPair> {
    let a = export("pkg/agent");
    let b = export("pkg/other");
    let key_a = bind(metadata, store, &a, "abc123").await?;
    let key_b = bind(metadata, store, &b, "abc123").await?;
    Ok(AgentPair {
        a: BoundAgent {
            export: a,
            key: key_a,
        },
        b: BoundAgent {
            export: b,
            key: key_b,
        },
    })
}

async fn assert_stored_tasks(
    store: &A2aStore,
    lookup: &Task,
    cases: &[(&Export, &Task)],
) -> Result<()> {
    for (export, task) in cases {
        assert_eq!(
            store
                .get_task_for_export(export, &alice(), &lookup.id)
                .await?
                .unwrap()
                .task,
            **task
        );
    }
    Ok(())
}

async fn assert_agent_dedupe_isolation(
    store: &A2aStore,
    agents: &AgentPair,
    task: &Task,
) -> Result<()> {
    store
        .put_message_dedupe(
            &agents.a.key,
            MessageIdentity {
                message_id: "message",
                fingerprint: "a",
            },
            &task.id,
        )
        .await?;
    assert!(store
        .get_message_dedupe(&agents.b.key, "message")
        .await?
        .is_none());
    store
        .put_message_dedupe(
            &agents.b.key,
            MessageIdentity {
                message_id: "message",
                fingerprint: "b",
            },
            &task.id,
        )
        .await?;
    for (key, fingerprint) in [(&agents.a.key, "a"), (&agents.b.key, "b")] {
        assert_eq!(
            store.get_message_dedupe(key, "message").await?.unwrap().1,
            fingerprint
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_same_local_id_is_isolated_by_agent() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let agents = bind_agent_pair(&metadata, &store).await?;
    assert_ne!(agents.a.key, agents.b.key);
    let task = snapshot("abc123");
    let record_a = create(&store, &agents.a.key, task.clone()).await?;
    assert!(store
        .get_task_for_export(&agents.b.export, &alice(), &task.id)
        .await?
        .is_none());
    let mut task_b = task.clone();
    task_b.artifacts = Some(vec![]);
    // Even the same task UUID must occupy separate storage keys.
    let record_b = create(&store, &agents.b.key, task_b).await?;
    assert_ne!(
        a2a_task_key(&agents.a.key, parse_task_id(&task.id)?.1),
        a2a_task_key(&agents.b.key, parse_task_id(&task.id)?.1)
    );
    assert_stored_tasks(
        &store,
        &task,
        &[
            (&agents.a.export, &record_a.task),
            (&agents.b.export, &record_b.task),
        ],
    )
    .await?;
    assert_agent_dedupe_isolation(&store, &agents, &task).await?;
    Ok(())
}

async fn seed_gc_records(store: &A2aStore, agents: &AgentPair, task: &Task) -> Result<()> {
    for agent in [&agents.a, &agents.b] {
        create(store, &agent.key, task.clone()).await?;
    }
    for agent in [&agents.a, &agents.b] {
        store
            .put_message_dedupe(
                &agent.key,
                MessageIdentity {
                    message_id: "message",
                    fingerprint: "fingerprint",
                },
                &task.id,
            )
            .await?;
    }
    Ok(())
}

async fn assert_gc_removed_records(
    metadata: &SessionMetadataStore,
    agent: &BoundAgent,
    task: &Task,
) -> Result<()> {
    let uuid = parse_task_id(&task.id)?.1;
    for (is_task, key) in [
        (true, a2a_task_key(&agent.key, uuid)),
        (
            false,
            a2a_message_key(&agent.key, &message_id_hash("message")),
        ),
    ] {
        let record = if is_task {
            metadata.get_a2a_task(&key).await?
        } else {
            metadata.get_a2a_message(&key).await?
        };
        assert!(record.is_none());
    }
    assert!(metadata.list_a2a_tasks(&agent.key).await?.is_empty());
    Ok(())
}

async fn assert_gc_removed_context(
    store: &A2aStore,
    agent: &BoundAgent,
    task: &Task,
) -> Result<()> {
    assert!(store.get_binding(&agent.key).await?.is_none());
    assert!(store
        .get_task_for_export(&agent.export, &alice(), &task.id)
        .await?
        .is_none());
    Ok(())
}

async fn assert_gc_survivor(store: &A2aStore, agent: &BoundAgent, task: &Task) -> Result<()> {
    assert_stored_tasks(store, task, &[(&agent.export, task)]).await?;
    assert_eq!(
        store
            .get_message_dedupe(&agent.key, "message")
            .await?
            .unwrap()
            .1,
        "fingerprint"
    );
    assert_eq!(
        list_tasks(store, &agent.export, &alice(), "abc123")
            .await?
            .unwrap()
            .len(),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_session_gc_removes_only_its_a2a_keys() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let agents = bind_agent_pair(&metadata, &store).await?;
    let task = snapshot("abc123");
    seed_gc_records(&store, &agents, &task).await?;
    // This is the exact primitive called by manual deletion and periodic GC.
    assert_eq!(metadata.purge_session_prefix(&agents.a.key).await?, 5); // meta, activity, task, message, index
    assert_gc_removed_records(&metadata, &agents.a, &task).await?;
    assert_gc_removed_context(&store, &agents.a, &task).await?;
    assert_gc_survivor(&store, &agents.b, &task).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_dedupe_hit_after_terminal_returns_existing_task() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let export = export("pkg/agent");
    let key = bind(&metadata, &store, &export, "abc123").await?;
    let record = create(&store, &key, snapshot("abc123")).await?;
    let message = &record.task.history.as_ref().unwrap()[0];
    let fingerprint = message_fingerprint(&message.parts);
    store
        .put_message_dedupe(
            &key,
            MessageIdentity {
                message_id: &message.message_id,
                fingerprint: &fingerprint,
            },
            &record.task.id,
        )
        .await?;
    let terminal = store
        .update_task(
            TaskVersion {
                storage_key: &key,
                task_id: &record.task.id,
                revision: record.revision,
            },
            TaskChanges {
                status: Some(completed()),
                artifacts: None,
                history: None,
            },
        )
        .await?;
    let restarted = A2aStore::new(metadata);
    let hit = restarted
        .dedupe_task(
            ContextAccess {
                export: &export,
                owner: &alice(),
                local_id: "abc123",
            },
            MessageIdentity {
                message_id: &message.message_id,
                fingerprint: &fingerprint,
            },
        )
        .await?
        .unwrap();
    assert_eq!(hit.task, terminal.task);
    assert_eq!(hit.revision, terminal.revision);
    assert!(hit.task.status.state.is_terminal());
    assert_eq!(
        list_tasks(&restarted, &export, &alice(), "abc123")
            .await?
            .unwrap()
            .len(),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_dedupe_fingerprint_mismatch_is_distinct_error() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let export = export("pkg/agent");
    let key = bind(&metadata, &store, &export, "abc123").await?;
    let record = create(&store, &key, snapshot("abc123")).await?;
    let original = message_fingerprint(&[Part::text("original")]);
    let changed = message_fingerprint(&[Part::text("changed")]);
    assert!(store
        .dedupe_task(
            ContextAccess {
                export: &export,
                owner: &alice(),
                local_id: "abc123"
            },
            MessageIdentity {
                message_id: "new",
                fingerprint: &original
            }
        )
        .await?
        .is_none());
    store
        .put_message_dedupe(
            &key,
            MessageIdentity {
                message_id: "message",
                fingerprint: &original,
            },
            &record.task.id,
        )
        .await?;
    let error = store
        .dedupe_task(
            ContextAccess {
                export: &export,
                owner: &alice(),
                local_id: "abc123",
            },
            MessageIdentity {
                message_id: "message",
                fingerprint: &changed,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<StoreError>(),
        Some(&StoreError::FingerprintMismatch)
    );
    assert!(store
        .put_message_dedupe(
            &key,
            MessageIdentity {
                message_id: "message",
                fingerprint: &changed
            },
            &record.task.id
        )
        .await
        .is_err());
    assert_eq!(
        store.get_message_dedupe(&key, "message").await?.unwrap().1,
        original
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_list_prefix_is_scoped_to_session() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let a = export("pkg/agent");
    let b = export("pkg/other");
    let key_a = bind(&metadata, &store, &a, "abc123").await?;
    let key_b = bind(&metadata, &store, &b, "abc123").await?;
    let other_context = bind(&metadata, &store, &a, "abc1234").await?;
    assert!(list_tasks(&store, &a, &alice(), "abc123")
        .await?
        .unwrap()
        .is_empty());
    let task_a = create(&store, &key_a, snapshot("abc123")).await?;
    let task_a2 = create(&store, &key_a, snapshot("abc123")).await?;
    create(&store, &key_b, snapshot("abc123")).await?;
    create(&store, &other_context, snapshot("abc1234")).await?;
    store
        .put_message_dedupe(
            &key_a,
            MessageIdentity {
                message_id: "message",
                fingerprint: "fingerprint",
            },
            &task_a.task.id,
        )
        .await?;
    // Deleted records must not appear in prefix listings.
    let deleted = create(&store, &key_a, snapshot("abc123")).await?;
    metadata
        .kv_store()
        .delete(a2a_task_key(&key_a, parse_task_id(&deleted.task.id)?.1))
        .await?;
    let mut expected = vec![task_a.task.id, task_a2.task.id];
    expected.sort();
    let actual = list_tasks(&store, &a, &alice(), "abc123").await?.unwrap();
    assert_eq!(
        actual
            .into_iter()
            .map(|record| record.task.id)
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(metadata.list_a2a_tasks(&key_a).await?.len(), 2);
    assert_eq!(
        list_tasks(&store, &b, &alice(), "abc123")
            .await?
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        list_tasks(&store, &a, &alice(), "abc1234")
            .await?
            .unwrap()
            .len(),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_binding_extension_is_valid_and_write_once() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let export = export("pkg/agent");
    let key = bind(&metadata, &store, &export, "abc123").await?;
    assert_eq!(key, session_key(Some("pkg/agent"), "abc123"));
    assert_ne!(key, session_key(Some("pkg/agent@test-cluster"), "abc123"));
    let mut record = metadata.get(&key).await?.unwrap();
    let binding = store.get_binding(&key).await?.unwrap();
    assert_eq!(
        record.metadata.extensions[A2A_BINDING_NAMESPACE],
        serde_json::to_value(&binding)?
    );
    assert_eq!(binding.owner.as_deref(), Some("alice"));
    // The public extension API rejects reserved/invalid namespaces, but accepts this one.
    assert!(metadata
        .replace_extension(&key, "bad/namespace", json!({}))
        .await
        .is_err());
    assert!(metadata
        .replace_extension(&key, "dev.harnx.execution_context", json!({}))
        .await
        .is_err());
    metadata
        .replace_extension(&key, A2A_BINDING_NAMESPACE, serde_json::to_value(&binding)?)
        .await?;
    let mut changed = binding.clone();
    changed.owner = Some("bob".into());
    assert!(store
        .write_binding(&key, &mut record, &changed)
        .await
        .is_err());
    assert_eq!(store.get_binding(&key).await?.unwrap().owner, binding.owner);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_new_context_lru_is_scoped_and_checks_fingerprint() -> Result<()> {
    use harnx_a2a_server::store::{DedupeEntry, DedupeKey};
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata);
    let key = DedupeKey {
        cluster: "cluster-a".into(),
        export: "export-a".into(),
        owner: Some("alice".into()),
        message_id: "message".into(),
    };
    assert!(store.check_dedupe_lru(&key, "fingerprint")?.is_none());
    store.record_dedupe_lru(
        key.clone(),
        DedupeEntry {
            task_id: "abc123.task".into(),
            fingerprint: "fingerprint".into(),
        },
    );
    assert_eq!(
        store.check_dedupe_lru(&key, "fingerprint")?.as_deref(),
        Some("abc123.task")
    );
    assert_eq!(
        store
            .check_dedupe_lru(&key, "changed")
            .unwrap_err()
            .downcast_ref::<StoreError>(),
        Some(&StoreError::FingerprintMismatch)
    );
    let mut other = key.clone();
    other.cluster = "cluster-b".into();
    assert!(store.check_dedupe_lru(&other, "fingerprint")?.is_none());
    other = key.clone();
    other.export = "export-b".into();
    assert!(store.check_dedupe_lru(&other, "fingerprint")?.is_none());
    other = key;
    other.owner = Some("bob".into());
    assert!(store.check_dedupe_lru(&other, "fingerprint")?.is_none());
    Ok(())
}

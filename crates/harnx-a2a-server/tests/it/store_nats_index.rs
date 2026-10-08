//! Task index maintenance, legacy migration, watch, and intent cleanup tests.
use super::store_nats::{
    alice, bind, completed, create, export, list_tasks, snapshot, start_store,
};
use crate::support::Broker;
use a2a_lf::{Task, TaskState, TaskStatus};
use anyhow::Result;
use chrono::Utc;
use harnx_a2a_server::store::{
    new_task_id, parse_task_id, A2aStore, IndexState, TaskChanges, TaskRecord, TaskSeed,
    TaskVersion,
};
use harnx_runtime::nats_session_metadata::{
    a2a_task_key, SessionMetadataStore, TaskIndex, TaskIndexEntry,
};
use serde_json::json;

async fn seed_legacy_task(
    metadata: &SessionMetadataStore,
    key: &str,
    task: &Task,
    seq: u64,
) -> Result<()> {
    let record = TaskRecord {
        version: 1,
        task: task.clone(),
        user_msg_id: format!("u{seq}"),
        user_msg_seq: seq,
        execution_id: format!("e{seq}"),
        revision: 1,
        stream_seq: 0,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let (_, uuid) = parse_task_id(&task.id)?;
    metadata
        .put_a2a_task(
            &a2a_task_key(key, uuid),
            serde_json::to_vec(&record)?.into(),
        )
        .await?;
    Ok(())
}

async fn assert_migrated_index(
    metadata: &SessionMetadataStore,
    key: &str,
    mut expected_ids: Vec<String>,
) -> Result<()> {
    let (migrated_index, _) = metadata
        .get_a2a_task_index(key)
        .await?
        .expect("migrated index");
    assert_eq!(migrated_index.entries.len(), expected_ids.len());
    expected_ids.sort();
    let index_ids: Vec<String> = migrated_index
        .entries
        .into_iter()
        .map(|e| e.task_id)
        .collect();
    assert_eq!(index_ids, expected_ids);
    Ok(())
}

async fn start_small_store(max_val: usize) -> Result<(Broker, SessionMetadataStore)> {
    harnx_core::require_nextest();
    let (broker, _, client) = Broker::start().await?;
    let js = async_nats::jetstream::new(client.clone());
    let small_kv = js
        .create_key_value(async_nats::jetstream::kv::Config {
            bucket: "small_val_test".to_string(),
            history: 1,
            max_value_size: max_val as i32,
            storage: async_nats::jetstream::stream::StorageType::File,
            ..Default::default()
        })
        .await?;
    let metadata = SessionMetadataStore::from_store(small_kv, client);
    Ok((broker, metadata))
}

fn submitted_seed(task_id: &str, context_id: &str, now: chrono::DateTime<Utc>) -> TaskSeed {
    TaskSeed {
        task: Task {
            id: task_id.to_string(),
            context_id: context_id.to_string(),
            status: TaskStatus {
                state: TaskState::Submitted,
                timestamp: Some(now),
                message: None,
            },
            history: None,
            artifacts: None,
            metadata: None,
        },
        user_msg_id: "msg-1".into(),
        user_msg_seq: 1,
        execution_id: "exec-1".into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_index_missing_vs_empty_and_one_time_migration() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let export = export("pkg/agent");
    let key = bind(&metadata, &store, &export, "empty_ctx").await?;

    // 1. Initially, index key is MISSING (None)
    assert!(metadata.get_a2a_task_index(&key).await?.is_none());

    // 2. Listing an empty session creates an EMPTY index (one-time migration of 0 tasks)
    let listed = list_tasks(&store, &export, &alice(), "empty_ctx")
        .await?
        .unwrap();
    assert!(listed.is_empty());

    // 3. Now the index is EMPTY, not MISSING
    let (index, rev) = metadata
        .get_a2a_task_index(&key)
        .await?
        .expect("index exists");
    assert!(index.entries.is_empty());
    assert!(rev > 0);

    // 4. Test legacy migration with existing tasks:
    let legacy_key = bind(&metadata, &store, &export, "legacy_ctx").await?;
    assert!(metadata.get_a2a_task_index(&legacy_key).await?.is_none());

    // Seed 2 tasks directly into KV (simulating legacy pre-index session)
    let legacy_t1 = snapshot("legacy_ctx");
    let legacy_t2 = snapshot("legacy_ctx");
    seed_legacy_task(&metadata, &legacy_key, &legacy_t1, 1).await?;
    seed_legacy_task(&metadata, &legacy_key, &legacy_t2, 2).await?;

    // Index is still missing
    assert!(metadata.get_a2a_task_index(&legacy_key).await?.is_none());

    // Calling list_tasks executes one-time migration CAS create
    let migrated = list_tasks(&store, &export, &alice(), "legacy_ctx")
        .await?
        .unwrap();
    assert_eq!(migrated.len(), 2);

    // Now index exists and contains both legacy entries
    assert_migrated_index(&metadata, &legacy_key, vec![legacy_t1.id, legacy_t2.id]).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_index_first_create_includes_legacy() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let export = export("pkg/agent");
    let key = bind(&metadata, &store, &export, "legacy_create").await?;

    // Seed 1 legacy task directly into KV
    let legacy_t = snapshot("legacy_create");
    seed_legacy_task(&metadata, &key, &legacy_t, 1).await?;

    // Verify index is missing
    assert!(metadata.get_a2a_task_index(&key).await?.is_none());

    // Create a new task via store.create_task: first create must include legacy task!
    let new_t = snapshot("legacy_create");
    let created = create(&store, &key, new_t.clone()).await?;

    // Index now exists and contains BOTH legacy and newly created tasks
    assert_migrated_index(&metadata, &key, vec![legacy_t.id, created.task.id]).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_index_consistency_missing_records_skipped() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let export = export("pkg/agent");
    let key = bind(&metadata, &store, &export, "consistency_test").await?;

    let t1 = create(&store, &key, snapshot("consistency_test")).await?;
    let t2 = create(&store, &key, snapshot("consistency_test")).await?;

    // Verify index has 2 entries
    let (index, _) = metadata
        .get_a2a_task_index(&key)
        .await?
        .expect("index exists");
    assert_eq!(index.entries.len(), 2);

    // Delete t1 directly from KV (simulating corruption / external deletion)
    metadata
        .kv_store()
        .delete(a2a_task_key(&key, parse_task_id(&t1.task.id)?.1))
        .await?;

    // Listing tasks skips missing records
    let listed = list_tasks(&store, &export, &alice(), "consistency_test")
        .await?
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].task.id, t2.task.id);

    // Keep the intent: a concurrent index-first creation may still write its record.
    let (repaired_index, _) = metadata
        .get_a2a_task_index(&key)
        .await?
        .expect("index exists");
    let t1_entry = repaired_index
        .entries
        .iter()
        .find(|e| e.task_id == t1.task.id)
        .unwrap();
    assert_eq!(t1_entry.state, IndexState::Working);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_index_consistency_repair_terminal_entries() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let export = export("pkg/agent");
    let key = bind(&metadata, &store, &export, "repair_test").await?;

    let t = create(&store, &key, snapshot("repair_test")).await?;

    // Initial state is Working
    let (index, _) = metadata
        .get_a2a_task_index(&key)
        .await?
        .expect("index exists");
    assert_eq!(index.entries[0].state, IndexState::Working);

    // Update the task record in KV to Completed directly (simulating crash before index was updated)
    let mut updated_record = t.clone();
    updated_record.task.status = completed();
    updated_record.revision += 1;
    let nats_rev = metadata
        .get_a2a_task(&a2a_task_key(&key, parse_task_id(&t.task.id)?.1))
        .await?
        .unwrap()
        .1;
    metadata
        .update_a2a_task(
            &a2a_task_key(&key, parse_task_id(&t.task.id)?.1),
            serde_json::to_vec(&updated_record)?.into(),
            nats_rev,
        )
        .await?;

    // Index still has it as Working
    let (index_before, _) = metadata
        .get_a2a_task_index(&key)
        .await?
        .expect("index exists");
    assert_eq!(index_before.entries[0].state, IndexState::Working);

    // Listing tasks repairs terminal entry in the index
    let listed = list_tasks(&store, &export, &alice(), "repair_test")
        .await?
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].task.status.state, TaskState::Completed);

    // Verify index entry was repaired to Completed
    let (index_after, _) = metadata
        .get_a2a_task_index(&key)
        .await?
        .expect("index exists");
    assert_eq!(index_after.entries[0].state, IndexState::Completed);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_index_bound_typed_cas_conflicts() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let export = export("pkg/agent");
    let key = bind(&metadata, &store, &export, "concurrency_test").await?;

    // Concurrently create tasks to exercise CAS retry under contention
    use std::sync::Arc;
    let store_arc = Arc::new(store);
    let key_arc = Arc::new(key);
    let handles: Vec<_> = (0..5)
        .map(|_| {
            let s = snapshot("concurrency_test");
            let s_clone = store_arc.clone();
            let k_clone = key_arc.clone();
            tokio::spawn(async move { create(&s_clone, &k_clone, s).await })
        })
        .collect();

    let mut created_ids = Vec::new();
    for h in handles {
        let rec = h.await??;
        created_ids.push(rec.task.id);
    }

    // Verify index has all 5 entries without conflict loss
    let (index, _) = metadata
        .get_a2a_task_index(&key_arc)
        .await?
        .expect("index exists");
    assert_eq!(index.entries.len(), 5);
    created_ids.sort();
    let index_ids: Vec<String> = index.entries.into_iter().map(|e| e.task_id).collect();
    assert_eq!(index_ids, created_ids);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_empty_index_never_rescans_legacy_keys() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let export = export("pkg/agent");
    let key = bind(&metadata, &store, &export, "empty-index").await?;
    assert!(store
        .get_or_migrate_index(&key, Some("empty-index"))
        .await?
        .0
        .entries
        .is_empty());
    // A legacy scan would attempt to deserialize this key and fail.
    let hidden = new_task_id("empty-index");
    metadata
        .put_a2a_task(
            &a2a_task_key(&key, parse_task_id(&hidden)?.1),
            "invalid json".into(),
        )
        .await?;
    for _ in 0..3 {
        assert!(list_tasks(&store, &export, &alice(), "empty-index")
            .await?
            .unwrap()
            .is_empty());
        assert!(store.list_non_terminal_tasks(&key).await?.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_index_repairs_preserve_status_timestamps_and_reject_stale_state() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let export = export("pkg/agent");
    let key = bind(&metadata, &store, &export, "timestamp-index").await?;
    let initial = create(&store, &key, snapshot("timestamp-index")).await?;
    let timestamp = Utc::now() - chrono::Duration::days(1);
    let mut record = store
        .update_task(
            TaskVersion {
                storage_key: &key,
                task_id: &initial.task.id,
                revision: initial.revision,
            },
            TaskChanges {
                status: Some(TaskStatus {
                    state: TaskState::Failed,
                    timestamp: Some(timestamp),
                    message: None,
                }),
                ..Default::default()
            },
        )
        .await?;
    // A late reader of Working must not revert the terminal index state.
    store.repair_index(&key, &initial).await?;
    let (index, _) = metadata.get_a2a_task_index(&key).await?.unwrap();
    assert_eq!(index.entries[0].state, IndexState::Failed);
    assert_eq!(index.entries[0].status_timestamp, Some(timestamp));
    assert_eq!(index.entries[0].task_revision, record.revision);
    for state in [
        TaskState::Unspecified,
        TaskState::AuthRequired,
        TaskState::Rejected,
        TaskState::InputRequired,
    ] {
        record = store
            .update_task(
                TaskVersion {
                    storage_key: &key,
                    task_id: &record.task.id,
                    revision: record.revision,
                },
                TaskChanges {
                    status: Some(TaskStatus {
                        state: state.clone(),
                        timestamp: None,
                        message: None,
                    }),
                    ..Default::default()
                },
            )
            .await?;
        let (index, _) = metadata.get_a2a_task_index(&key).await?.unwrap();
        assert!(harnx_a2a_server::handler::task_view::entry_matches(
            &index.entries[0],
            &serde_json::from_value::<a2a_lf::ListTasksRequest>(json!({"status": state}))?
        ));
        assert_eq!(index.entries[0].status_timestamp, None);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_task_watch_pushes_updates_and_reread_handles_already_terminal() -> Result<()> {
    use futures::StreamExt;
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let export = export("pkg/agent");
    let key = bind(&metadata, &store, &export, "task-watch").await?;
    let record = create(&store, &key, snapshot("task-watch")).await?;
    let mut watch = store.watch_task(&key, &record.task.id).await?;
    store
        .update_task(
            TaskVersion {
                storage_key: &key,
                task_id: &record.task.id,
                revision: record.revision,
            },
            TaskChanges {
                status: Some(TaskStatus {
                    state: TaskState::Completed,
                    timestamp: None,
                    message: None,
                }),
                ..Default::default()
            },
        )
        .await?;
    tokio::time::timeout(crate::support::DEADLINE, watch.next())
        .await?
        .unwrap()?;
    assert!(store
        .get_task(&key, &record.task.id)
        .await?
        .unwrap()
        .task
        .status
        .state
        .is_terminal());
    // New-only KV watches won't replay completion. Re-read after subscribing.
    let _watch = store.watch_task(&key, &record.task.id).await?;
    assert!(store
        .get_task(&key, &record.task.id)
        .await?
        .unwrap()
        .task
        .status
        .state
        .is_terminal());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_index_cleanup_expired_vs_recent_intents() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let export = export("pkg/agent");
    let local_id = "grace-test";
    let key = bind(&metadata, &store, &export, local_id).await?;

    let now = Utc::now();
    let expired_id = new_task_id(local_id);
    let mut expired_entry = TaskIndexEntry::new(
        expired_id.clone(),
        IndexState::Working,
        now - chrono::Duration::seconds(360),
    );
    expired_entry.updated_at = now - chrono::Duration::seconds(360);
    expired_entry.task_revision = 1;

    let recent_id = new_task_id(local_id);
    let mut recent_entry = TaskIndexEntry::new(
        recent_id.clone(),
        IndexState::Working,
        now - chrono::Duration::seconds(60),
    );
    recent_entry.updated_at = now - chrono::Duration::seconds(60);
    recent_entry.task_revision = 1;

    let mut index = TaskIndex::new();
    index.add(expired_entry);
    index.add(recent_entry);
    metadata.put_a2a_task_index(&key, &index, None).await?;

    let non_terminal = store.list_non_terminal_entries(&key).await?;
    assert_eq!(non_terminal.len(), 2);

    // Expired entry is cleaned up
    let removed_expired = store
        .cleanup_expired_task_index_entry(&key, &expired_id, 1)
        .await?;
    assert!(removed_expired);

    assert!(
        !store
            .cleanup_expired_task_index_entry(&key, &recent_id, 1)
            .await?
    );

    // Recent entry remains in index (grace period preserved)
    let (index_after, _) = metadata.get_a2a_task_index(&key).await?.unwrap();
    assert_eq!(index_after.entries.len(), 1);
    assert_eq!(index_after.entries[0].task_id, recent_id);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_index_stale_cleanup_fences() -> Result<()> {
    let (_broker, metadata) = start_store().await?;
    let store = A2aStore::new(metadata.clone());
    let export = export("pkg/agent");
    let local_id = "fence-test";
    let key = bind(&metadata, &store, &export, local_id).await?;

    let now = Utc::now();
    let task_id = new_task_id(local_id);
    let mut entry = TaskIndexEntry::new(
        task_id.clone(),
        IndexState::Working,
        now - chrono::Duration::seconds(400),
    );
    entry.updated_at = now - chrono::Duration::seconds(400);
    entry.task_revision = 2;

    let mut index = TaskIndex::new();
    index.add(entry);
    metadata.put_a2a_task_index(&key, &index, None).await?;

    // Stale expected_revision = 1 must be rejected by fence
    let removed = store
        .cleanup_expired_task_index_entry(&key, &task_id, 1)
        .await?;
    assert!(!removed);

    // Entry remains in the index
    let (current_index, _) = metadata.get_a2a_task_index(&key).await?.unwrap();
    assert_eq!(current_index.entries.len(), 1);
    assert_eq!(current_index.entries[0].task_revision, 2);

    // Transition to terminal state (Completed at revision 3)
    let (current_index, current_rev) = metadata.get_a2a_task_index(&key).await?.unwrap();
    let mut terminal_entry = current_index.entries[0].clone();
    terminal_entry.state = IndexState::Completed;
    terminal_entry.task_revision = 3;
    let mut updated_index = current_index;
    updated_index.add(terminal_entry);
    metadata
        .put_a2a_task_index(&key, &updated_index, Some(current_rev))
        .await?;

    // Attempting cleanup on terminal entry must also abort
    let removed_term = store
        .cleanup_expired_task_index_entry(&key, &task_id, 3)
        .await?;
    assert!(!removed_term);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_index_size_limit_clear_error_does_not_write_task() -> Result<()> {
    let (_broker, metadata) = start_small_store(512).await?;
    let store = A2aStore::new(metadata.clone());
    let export = export("pkg/agent");
    let local_id = "size-limit-ctx";
    let key = bind(&metadata, &store, &export, local_id).await?;

    // Fill compact index near the configured 512-byte limit.
    let now = Utc::now();
    let mut index = TaskIndex::new();
    for i in 0..3 {
        index.add(TaskIndexEntry::new(
            format!("{local_id}.existing-task-{i}"),
            IndexState::Completed,
            now,
        ));
    }
    metadata.put_a2a_task_index(&key, &index, None).await?;

    // Creating a new task will exceed 512 bytes
    let task_id = new_task_id(local_id);
    let seed = submitted_seed(&task_id, local_id, now);

    let result = store.create_task(&key, seed).await;
    assert!(result.is_err());
    let err_str = result.err().unwrap().to_string();
    assert!(
        err_str.contains("task index exceeds maximum size limit"),
        "unexpected error message: {err_str}"
    );

    // Verify task record was NOT written to KV
    let task_record = store.get_task(&key, &task_id).await?;
    assert!(
        task_record.is_none(),
        "task record should NOT have been written"
    );

    Ok(())
}

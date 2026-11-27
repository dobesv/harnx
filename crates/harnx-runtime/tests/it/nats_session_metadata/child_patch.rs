//! Broker-backed tests for the parent-supplied child metadata patch
//! (`nats_session_metadata::child_patch`): the first canonical record a
//! create persists, the revision-guarded existing-session CAS, and the
//! recoverable conflict semantics a retried invocation relies on.
use crate::common::{spawn_nats_server, NatsServerHandle};
use anyhow::Result;
use harnx_core::require_nextest;
use harnx_runtime::nats_session_metadata::{
    apply_to_initializer, metadata_key, session_properties, ChildPatchOutcome,
    ChildSessionMetadataPatch, MetadataRecord, SessionInitializer, SessionMetadata,
    SessionMetadataStore, SESSION_PROPERTIES_NAMESPACE,
};
use serde_json::{json, Value};

// qual:test_helper — shared fixture for this module and the race module below.
pub(super) fn child_patch(value: Value) -> ChildSessionMetadataPatch {
    serde_json::from_value(value).unwrap()
}

/// Broker fixture for this module. `None` means no NATS server is available
/// and the test returns early. The handle must stay bound for the whole
/// test: dropping it kills the broker.
// qual:test_helper — shared fixture for this module and the race module below.
pub(super) async fn fixture() -> Result<Option<(NatsServerHandle, SessionMetadataStore)>> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(None);
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    Ok(Some((server, store)))
}

// qual:test_helper — seed plus authoritative re-read in one step.
pub(super) async fn seeded(
    store: &SessionMetadataStore,
    prefix: &str,
) -> Result<(String, MetadataRecord)> {
    let local_id = format!("{prefix}-{}", uuid::Uuid::new_v4());
    let metadata = SessionMetadata::new(
        &local_id,
        SessionInitializer::named("metis", Default::default()),
    );
    let storage_key = metadata.storage_key();
    store
        .create(&metadata)
        .await?
        .expect("fresh session created");
    let record = store.get(&storage_key).await?.expect("metadata exists");
    Ok((storage_key, record))
}

// qual:test_helper — one-line pinned apply, so the CAS tests share a single
// call shape instead of repeating the store invocation.
pub(super) async fn applied_at(
    store: &SessionMetadataStore,
    storage_key: &str,
    patch: &ChildSessionMetadataPatch,
    expected_revision: u64,
) -> Result<ChildPatchOutcome> {
    store
        .apply_child_patch(storage_key, patch, Some(expected_revision))
        .await
}

// qual:test_helper — seed plus parsed patch in one step, for tests whose
// next statement would otherwise repeat the same two-line preamble.
async fn seeded_with_patch(
    store: &SessionMetadataStore,
    prefix: &str,
    patch: Value,
) -> Result<(String, MetadataRecord, ChildSessionMetadataPatch)> {
    let (storage_key, record) = seeded(store, prefix).await?;
    Ok((storage_key, record, child_patch(patch)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_patch_lands_in_the_first_canonical_record() -> Result<()> {
    let Some((_server, store)) = fixture().await? else {
        return Ok(());
    };
    let local_id = format!("child-create-{}", uuid::Uuid::new_v4());
    let mut initializer = SessionInitializer::named("metis", Default::default());
    let patch = child_patch(json!({
        "title": {"value": "  Refactor the parser  ", "manual": true},
        "variables": {"priority": "high"},
        "properties": {
            "set": [{"name": "git_branch", "value": "dobesv/feature"}],
            "add_labels": ["metadata"]
        }
    }));
    apply_to_initializer(&mut initializer, &patch)?;
    let metadata = SessionMetadata::new(&local_id, initializer);
    let storage_key = metadata.storage_key();
    let revision = store
        .create(&metadata)
        .await?
        .expect("fresh session created");

    // Observe the first record the broker actually holds.
    let entry = store
        .kv_store()
        .entry(metadata_key(&storage_key))
        .await?
        .expect("first record exists");
    assert_eq!(entry.revision, revision);
    assert_eq!(entry.operation, async_nats::jetstream::kv::Operation::Put);
    let stored: Value = serde_json::from_slice(&entry.value)?;
    assert_eq!(stored["title"]["value"], json!("Refactor the parser"));
    assert!(stored["title"]["manual"].as_bool().unwrap());
    assert_eq!(stored["variables"], json!({"priority": "high"}));
    let properties = &stored["extensions"][SESSION_PROPERTIES_NAMESPACE];
    assert_eq!(properties["git_branch"]["value"], json!("dobesv/feature"));
    assert_eq!(properties["labels"]["value"], json!(["metadata"]));

    let record = store.get(&storage_key).await?.expect("metadata exists");
    assert_eq!(record.revision, revision);
    assert_eq!(
        record.metadata.title.value.as_deref(),
        Some("Refactor the parser")
    );
    assert!(record.metadata.title.manual);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_patch_applies_at_the_expected_revision_and_preserves_unrelated_state() -> Result<()>
{
    let Some((_server, store)) = fixture().await? else {
        return Ok(());
    };
    let (storage_key, _) = seeded(&store, "child-update").await?;
    // Unrelated state the patch must carry along untouched.
    store
        .patch(&storage_key, |metadata| {
            metadata.overrides.model = Some("m1".to_string());
            metadata
                .extensions
                .insert("example.client".to_string(), json!({"keep": true}));
            metadata
                .variables
                .insert("stored".to_string(), "v".to_string());
            Ok(())
        })
        .await?;
    let record = store.get(&storage_key).await?.expect("metadata exists");

    let patch = child_patch(json!({
        "title": {"value": "Parent title"},
        "variables": {"only": "this"},
        "properties": {"set": [{"name": "github_issue", "value": 2970}]}
    }));
    match applied_at(&store, &storage_key, &patch, record.revision).await? {
        ChildPatchOutcome::Applied(updated) => {
            assert!(updated.revision > record.revision);
            assert_eq!(
                updated.metadata.title.value.as_deref(),
                Some("Parent title")
            );
            assert_eq!(
                updated.metadata.variables.get("only").map(String::as_str),
                Some("this")
            );
            assert!(updated.metadata.variables.get("stored").is_none());
            assert_eq!(
                session_properties(&updated.metadata)?
                    .get("github_issue")
                    .expect("github_issue set")
                    .value,
                json!(2970)
            );
            assert_eq!(updated.metadata.overrides.model.as_deref(), Some("m1"));
            assert_eq!(
                updated.metadata.extensions.get("example.client"),
                Some(&json!({"keep": true}))
            );
            assert_eq!(updated.metadata.created_at, record.metadata.created_at);
        }
        other => panic!("expected the patch to apply, got {other:?}"),
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_patch_conflict_cannot_overwrite_newer_state() -> Result<()> {
    let Some((_server, store)) = fixture().await? else {
        return Ok(());
    };
    let (storage_key, record) = seeded(&store, "child-conflict").await?;

    // A later child edit moves the record past the pinned revision.
    store
        .patch(&storage_key, |metadata| {
            metadata.title.value = Some("Child edit".to_string());
            Ok(())
        })
        .await?;

    let patch = child_patch(json!({"title": {"value": "Parent title"}}));
    match applied_at(&store, &storage_key, &patch, record.revision).await? {
        ChildPatchOutcome::Conflict {
            current,
            expected_revision,
        } => {
            assert_eq!(expected_revision, record.revision);
            assert_eq!(current.metadata.title.value.as_deref(), Some("Child edit"));
            assert!(current.revision > record.revision);
            let after = store.get(&storage_key).await?.expect("metadata exists");
            assert_eq!(after.revision, current.revision);
            assert_eq!(after.metadata.title.value.as_deref(), Some("Child edit"));
        }
        other => panic!("expected a recoverable conflict, got {other:?}"),
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_patch_already_held_satisfies_a_stale_revision_without_writing() -> Result<()> {
    let Some((_server, store)) = fixture().await? else {
        return Ok(());
    };
    let (storage_key, record, patch) =
        seeded_with_patch(&store, "child-held", json!({"title": {"value": "Target"}})).await?;
    let applied_revision = match applied_at(&store, &storage_key, &patch, record.revision).await? {
        ChildPatchOutcome::Applied(updated) => updated.revision,
        other => panic!("expected the patch to apply, got {other:?}"),
    };

    // The same invocation retries while still pinned to the original
    // revision: the state already holds, so nothing is written over the
    // record's later state.
    match applied_at(&store, &storage_key, &patch, record.revision).await? {
        ChildPatchOutcome::AlreadyHeld(held) => {
            assert_eq!(held.revision, applied_revision);
            assert_eq!(held.metadata.title.value.as_deref(), Some("Target"));
        }
        other => panic!("expected the state to already hold, got {other:?}"),
    }
    let after = store.get(&storage_key).await?.expect("metadata exists");
    assert_eq!(after.revision, applied_revision);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_patch_with_invalid_properties_changes_nothing() -> Result<()> {
    let Some((_server, store)) = fixture().await? else {
        return Ok(());
    };
    let (storage_key, record, patch) = seeded_with_patch(
        &store,
        "child-invalid",
        json!({
            "title": {"value": "would-be title"},
            "properties": {"set": [{"name": "github_issue", "value": "not-a-number"}]}
        }),
    )
    .await?;
    assert!(applied_at(&store, &storage_key, &patch, record.revision)
        .await
        .is_err());

    let after = store.get(&storage_key).await?.expect("metadata exists");
    assert_eq!(after.revision, record.revision);
    assert_eq!(after.metadata.title.value, None);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_patch_on_a_missing_record_is_an_error() -> Result<()> {
    let Some((_server, store)) = fixture().await? else {
        return Ok(());
    };
    let storage_key = harnx_core::session_identity::session_key(
        Some("metis"),
        &format!("child-missing-{}", uuid::Uuid::new_v4()),
    );
    let patch = child_patch(json!({"title": {"value": "Parent title"}}));
    let error = store
        .apply_child_patch(&storage_key, &patch, None)
        .await
        .expect_err("missing metadata record");
    assert!(
        error.to_string().contains("not found"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

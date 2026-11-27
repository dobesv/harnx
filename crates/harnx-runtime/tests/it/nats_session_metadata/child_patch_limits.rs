//! Extension-size regressions for the parent-supplied child metadata patch:
//! the pinned CAS must refuse over-limit properties (namespace and
//! aggregate) without writing, and a stale pinned retry must surface a
//! recoverable conflict even when the newer record would reject the patch.
use super::child_patch::{applied_at, child_patch, fixture, seeded};
use anyhow::Result;
use harnx_core::require_nextest;
use harnx_runtime::nats_session_metadata::{
    ChildPatchOutcome, ChildSessionMetadataPatch, SessionInitializer, SessionMetadata,
    SessionMetadataStore, SessionProperties, CUSTOM_PROPERTIES_MAX,
};
use serde_json::json;

/// A properties patch of `count` custom values of 2040 bytes. Every
/// per-property rule passes — the values fit `CUSTOM_VALUE_MAX_BYTES` and the
/// count fits `CUSTOM_PROPERTIES_MAX` — so only the serialized extension
/// limits can reject it.
fn bulky_properties(count: usize) -> ChildSessionMetadataPatch {
    let set = (0..count)
        .map(|index| json!({"name": format!("c{index:02}"), "value": "x".repeat(2040)}))
        .collect::<Vec<_>>();
    child_patch(json!({"properties": {"set": set}}))
}

// qual:test_helper — `count` inherited custom properties, without values.
fn custom_properties(count: usize) -> SessionProperties {
    let properties: serde_json::Map<String, serde_json::Value> = (0..count)
        .map(|index| {
            (
                format!("c{index:02}"),
                json!({"value": format!("v{index}"), "inherit": false}),
            )
        })
        .collect();
    serde_json::from_value(json!(properties)).unwrap()
}

/// Create the session described by `metadata` and return its storage key
/// with the current record, mirroring what T2's creation path will do.
async fn created(
    store: &SessionMetadataStore,
    metadata: SessionMetadata,
) -> Result<(String, harnx_runtime::nats_session_metadata::MetadataRecord)> {
    let storage_key = metadata.storage_key();
    store
        .create(&metadata)
        .await?
        .expect("fresh session created");
    let record = store.get(&storage_key).await?.expect("metadata exists");
    Ok((storage_key, record))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_patch_pinned_rejects_extension_overflow_without_writing() -> Result<()> {
    require_nextest();
    let Some((_server, store)) = fixture().await? else {
        return Ok(());
    };
    let (storage_key, record) = seeded(&store, "child-overflow").await?;

    // 32 x 2040-byte values overflow the 64 KiB properties namespace even
    // though each property rule passes.
    let patch = bulky_properties(CUSTOM_PROPERTIES_MAX);
    assert!(applied_at(&store, &storage_key, &patch, record.revision)
        .await
        .is_err());

    // Nothing was written: the record keeps its revision and still decodes.
    let after = store.get(&storage_key).await?.expect("metadata exists");
    assert_eq!(after.revision, record.revision);
    assert_eq!(after.metadata, record.metadata);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_patch_pinned_rejects_aggregate_extension_overflow_without_writing() -> Result<()> {
    require_nextest();
    let Some((_server, store)) = fixture().await? else {
        return Ok(());
    };
    // Four client namespaces of ~50 KB each stay valid at creation; a patch
    // whose properties fit their own 64 KiB namespace then pushes the total
    // past the 256 KiB aggregate limit.
    let mut metadata = SessionMetadata::new(
        format!("child-bulk-{}", uuid::Uuid::new_v4()),
        SessionInitializer::named("metis", Default::default()),
    );
    for index in 0..4 {
        metadata.extensions.insert(
            format!("example.n{index}"),
            json!({"v": "x".repeat(50_000)}),
        );
    }
    let (storage_key, record) = created(&store, metadata).await?;

    let patch = bulky_properties(31);
    assert!(applied_at(&store, &storage_key, &patch, record.revision)
        .await
        .is_err());

    let after = store.get(&storage_key).await?.expect("metadata exists");
    assert_eq!(after.revision, record.revision);
    assert_eq!(after.metadata, record.metadata);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_patch_conflict_surfaces_when_newer_state_would_reject_the_patch() -> Result<()> {
    require_nextest();
    let Some((_server, store)) = fixture().await? else {
        return Ok(());
    };
    // A record already at the custom-property count limit.
    let metadata = SessionMetadata::new(
        format!("child-full-{}", uuid::Uuid::new_v4()),
        SessionInitializer::named("metis", Default::default())
            .with_properties(custom_properties(CUSTOM_PROPERTIES_MAX)),
    );
    let (storage_key, record) = created(&store, metadata).await?;
    let patch = child_patch(json!({
        "properties": {"set": [{"name": "overflow", "value": "v"}]}
    }));

    // At the pinned revision, the over-count patch is a genuine validation
    // error and writes nothing.
    assert!(applied_at(&store, &storage_key, &patch, record.revision)
        .await
        .is_err());

    // A later child edit moves the record on. The stale retry must surface
    // the recoverable conflict — the caller re-checks admission — never the
    // count-limit error the newer state would produce.
    store
        .patch(&storage_key, |metadata| {
            metadata.title.value = Some("Child edit".to_string());
            Ok(())
        })
        .await?;
    match applied_at(&store, &storage_key, &patch, record.revision).await? {
        ChildPatchOutcome::Conflict {
            current,
            expected_revision,
        } => {
            assert_eq!(expected_revision, record.revision);
            assert!(current.revision > record.revision);
            assert_eq!(current.metadata.title.value.as_deref(), Some("Child edit"));
        }
        other => panic!("expected a recoverable conflict, got {other:?}"),
    }
    Ok(())
}

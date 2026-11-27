//! The concurrent-writer race for the parent-supplied child metadata patch:
//! a fresh (unpinned) apply must merge with other metadata writers instead
//! of clobbering them or losing its own fields.
use super::child_patch::{child_patch, fixture, seeded};
use anyhow::Result;
use harnx_runtime::nats_session_metadata::ChildPatchOutcome;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_patch_fresh_apply_merges_with_concurrent_writers() -> Result<()> {
    let Some((_server, store)) = fixture().await? else {
        return Ok(());
    };
    let (storage_key, _) = seeded(&store, "child-race").await?;

    // Five concurrent extension writers plus the child patch, released
    // together. The store's metadata mutations retry lost revision races a
    // bounded number of times, so the contending writes must stay within that
    // budget (see CAS_RETRY_LIMIT and concurrent_title_updates_retry_cas_conflicts).
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(6));
    let mut writers = Vec::new();
    for index in 0..5 {
        let writer_store = store.clone();
        let writer_session = storage_key.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        writers.push(tokio::spawn(async move {
            barrier.wait().await;
            writer_store
                .patch(&writer_session, |metadata| {
                    metadata
                        .extensions
                        .insert(format!("example.step{index}"), json!(true));
                    Ok(())
                })
                .await
        }));
    }
    let patch_store = store.clone();
    let patch_session = storage_key.clone();
    let barrier = std::sync::Arc::clone(&barrier);
    let patch = child_patch(json!({
        "title": {"value": "Parent title"},
        "variables": {"only": "this"},
        "properties": {"set": [{"name": "git_branch", "value": "dobesv/feature"}]}
    }));
    let applied = tokio::spawn(async move {
        barrier.wait().await;
        patch_store
            .apply_child_patch(&patch_session, &patch, None)
            .await
    });
    for writer in writers {
        writer.await??;
    }
    match applied.await?? {
        ChildPatchOutcome::Applied(_) => {}
        other => panic!("expected the patch to apply, got {other:?}"),
    }

    let final_record = store.get(&storage_key).await?.expect("metadata exists");
    // The patch's fields and every concurrent writer's extension survive.
    assert_eq!(
        final_record.metadata.title.value.as_deref(),
        Some("Parent title")
    );
    assert_eq!(
        final_record
            .metadata
            .variables
            .get("only")
            .map(String::as_str),
        Some("this")
    );
    assert_eq!(
        harnx_runtime::nats_session_metadata::session_properties(&final_record.metadata)?
            .get("git_branch")
            .expect("git_branch set")
            .value,
        json!("dobesv/feature")
    );
    for index in 0..5 {
        assert_eq!(
            final_record
                .metadata
                .extensions
                .get(&format!("example.step{index}")),
            Some(&json!(true))
        );
    }
    Ok(())
}

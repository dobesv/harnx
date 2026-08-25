mod common;

use anyhow::Result;
use common::spawn_nats_server;
use harnx_core::require_nextest;
use harnx_runtime::nats_session_metadata::{
    read_cursor_key, SessionExtensionUpdate, SessionInitializer, SessionMetadata,
    SessionMetadataPatch, SessionMetadataStore, SessionOverrideUpdate,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_and_activity_cas_updates_do_not_contend() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let session_id = format!("metadata-activity-{}", uuid::Uuid::new_v4());
    store
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named("metis", Default::default()),
        ))
        .await?;

    let title_store = store.clone();
    let title_session = session_id.clone();
    let title = tokio::spawn(async move {
        for index in 0..12 {
            title_store
                .patch(&title_session, |metadata| {
                    metadata.title.value = Some(format!("title-{index}"));
                    Ok(())
                })
                .await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    let activity_store = store.clone();
    let activity_session = session_id.clone();
    let activity = tokio::spawn(async move {
        for _ in 0..24 {
            activity_store.touch_activity(&activity_session).await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    title.await??;
    activity.await??;

    let record = store.get(&session_id).await?.expect("metadata exists");
    assert_eq!(record.metadata.title.value.as_deref(), Some("title-11"));
    assert!(store.get_activity(&session_id).await?.is_some());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn purge_removes_metadata_activity_extensions_and_future_cursors() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let session_id = format!("metadata-purge-{}", uuid::Uuid::new_v4());
    store
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named("metis", Default::default()),
        ))
        .await?;
    store
        .replace_extension(
            &session_id,
            "example.namespace",
            serde_json::json!({"v": 1}),
        )
        .await?;
    store
        .kv_store()
        .put(read_cursor_key(&session_id, "viewer"), "12".into())
        .await?;

    let keys = [
        harnx_runtime::nats_session_metadata::metadata_key(&session_id),
        harnx_runtime::nats_session_metadata::activity_key(&session_id),
        read_cursor_key(&session_id, "viewer"),
    ];
    assert_eq!(store.purge_session_prefix(&session_id).await?, keys.len());
    assert!(store.get(&session_id).await?.is_none());
    assert!(store.get_activity(&session_id).await?.is_none());
    assert!(store
        .kv_store()
        .get(read_cursor_key(&session_id, "viewer"))
        .await?
        .is_none());
    for key in keys {
        let entry = store
            .kv_store()
            .entry(key)
            .await?
            .expect("purge marker remains");
        assert_eq!(entry.operation, async_nats::jetstream::kv::Operation::Purge);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_title_updates_retry_cas_conflicts() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let session_id = format!("metadata-title-race-{}", uuid::Uuid::new_v4());
    store
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named("metis", Default::default()),
        ))
        .await?;

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(6));
    let mut updates = Vec::new();
    for index in 0..6 {
        let store = store.clone();
        let session_id = session_id.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        updates.push(tokio::spawn(async move {
            barrier.wait().await;
            store
                .patch(&session_id, |metadata| {
                    metadata.title.value = Some(format!("title-{index}"));
                    Ok(())
                })
                .await
        }));
    }
    for update in updates {
        update.await??;
    }

    let record = store.get(&session_id).await?.expect("metadata exists");
    let title = record.metadata.title.value.expect("a title won the race");
    assert!(title.starts_with("title-"));
    assert!(record.revision >= 7, "create plus six patches must commit");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_override_fields_are_merged() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let session_id = format!("metadata-overrides-{}", uuid::Uuid::new_v4());
    store
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named("metis", Default::default()),
        ))
        .await?;

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let model_update = {
        let store = store.clone();
        let session_id = session_id.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            store
                .apply_override(
                    &session_id,
                    SessionOverrideUpdate::Model(Some("openai:gpt-5".to_string())),
                )
                .await
        })
    };
    let temperature_update = {
        let store = store.clone();
        let session_id = session_id.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            store
                .apply_override(&session_id, SessionOverrideUpdate::Temperature(Some(0.4)))
                .await
        })
    };
    model_update.await??;
    temperature_update.await??;

    let record = store.get(&session_id).await?.expect("metadata exists");
    assert_eq!(
        record.metadata.overrides.model.as_deref(),
        Some("openai:gpt-5")
    );
    assert_eq!(record.metadata.overrides.temperature, Some(0.4));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_worker_fence_cannot_overwrite_metadata() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let session_id = format!("metadata-fence-{}", uuid::Uuid::new_v4());
    store
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named("metis", Default::default()),
        ))
        .await?;

    store
        .patch_with_fence(&session_id, 20, |metadata| {
            metadata.title.value = Some("new worker".to_string());
            Ok(())
        })
        .await?;
    // A client-side patch must preserve the internal fence.
    store
        .apply_patch(&session_id, SessionMetadataPatch::default())
        .await?;
    let error = store
        .patch_with_fence(&session_id, 19, |metadata| {
            metadata.title.value = Some("stale worker".to_string());
            Ok(())
        })
        .await
        .expect_err("an older worker fence must be rejected");
    assert!(error.to_string().contains("stale session metadata writer"));

    let record = store.get(&session_id).await?.expect("metadata exists");
    assert_eq!(record.metadata.title.value.as_deref(), Some("new worker"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_bound_mutations_hide_other_agents_sessions() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let session_id = format!("metadata-agent-{}", uuid::Uuid::new_v4());
    store
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named("metis", Default::default()),
        ))
        .await?;

    assert!(store
        .get_for_agent(&session_id, "aristarchus")
        .await?
        .is_none());
    let error = store
        .apply_patch_for_agent(&session_id, "aristarchus", SessionMetadataPatch::default())
        .await
        .expect_err("another agent must not mutate the session");
    assert_eq!(error.to_string(), "Not Found");
    store
        .replace_extension_for_agent(
            &session_id,
            "metis",
            SessionExtensionUpdate {
                namespace: "client",
                value: serde_json::json!({"ok": true}),
            },
        )
        .await?;
    Ok(())
}

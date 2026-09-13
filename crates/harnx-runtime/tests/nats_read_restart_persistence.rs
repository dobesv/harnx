//! Integration test for restart persistence of read-state.
//!
//! Tests that unread state persists across process/store restart.

mod common;

use anyhow::Result;
use common::spawn_nats_server;
use harnx_core::require_nextest;
use harnx_runtime::nats_session_metadata::{
    SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use std::time::Duration;

fn new_remote_session_id() -> String {
    format!("restart-session-{}", uuid::Uuid::new_v4())
}

/// Read-state persists across store handle recreation.
///
/// Simulates process restart by dropping the store handle and creating a new one.
#[tokio::test]
async fn unread_state_persists_across_store_restart() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());

    let session_id = new_remote_session_id();

    // Create store and session
    {
        let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
        store
            .create(&SessionMetadata::new(
                &session_id,
                SessionInitializer::named("test-agent", Default::default()),
            ))
            .await?;

        // Bump attention
        store.bump_attention(&session_id, 10).await?;

        // Verify unread
        let state = store.get_read_state(&session_id).await?;
        assert!(state.is_unread(), "should be unread after attention bump");
    }
    // Store handle dropped (simulates process exit)

    // Wait for cleanup
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create new store handle (simulates process restart)
    {
        let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
        let state = store.get_read_state(&session_id).await?;
        assert!(
            state.is_unread(),
            "unread state should persist after restart"
        );
        assert_eq!(state.last_attention_seq, 10);
    }

    Ok(())
}

/// Manual_unread flag persists across store restart.
#[tokio::test]
async fn manual_unread_flag_persists_across_restart() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());

    let session_id = new_remote_session_id();

    // Create and set up unread
    {
        let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
        store
            .create(&SessionMetadata::new(
                &session_id,
                SessionInitializer::named("test-agent", Default::default()),
            ))
            .await?;

        // Bump attention and mark read
        store.bump_attention(&session_id, 5).await?;
        store.mark_read(&session_id).await?;

        // Mark manual unread
        store.mark_unread(&session_id).await?;

        let state = store.get_read_state(&session_id).await?;
        assert!(state.is_unread());
        assert!(state.manual_unread);
    }

    // Wait and create new store
    tokio::time::sleep(Duration::from_millis(100)).await;

    {
        let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
        let state = store.get_read_state(&session_id).await?;
        assert!(state.is_unread(), "manual_unread should persist");
        assert!(
            state.manual_unread,
            "manual_unread flag should persist after restart"
        );
    }

    Ok(())
}

/// Read-state survives KV bucket recreation (data in NATS persists).
#[tokio::test]
async fn read_state_survives_bucket_recreate() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());

    let session_id = new_remote_session_id();

    // Create initial state
    {
        let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
        store
            .create(&SessionMetadata::new(
                &session_id,
                SessionInitializer::named("test-agent", Default::default()),
            ))
            .await?;

        store.bump_attention(&session_id, 42).await?;
    }

    // Drop and wait
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Re-ensure the bucket (idempotent, already exists)
    {
        let store = SessionMetadataStore::ensure(&jetstream, 1).await?;

        // Data should still be there
        let state = store.get_read_state(&session_id).await?;
        assert_eq!(
            state.last_attention_seq, 42,
            "attention seq should survive bucket re-ensure"
        );
        assert!(state.is_unread());
    }

    Ok(())
}

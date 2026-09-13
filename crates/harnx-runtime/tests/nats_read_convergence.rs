//! Integration tests for two-client read-state convergence.
//!
//! Tests that two clients concurrently modifying read-state converge to the
//! last-writer state, and that revision ordering prevents a stale bool overwrite.

mod common;

use anyhow::Result;
use common::spawn_nats_server;
use futures_util::StreamExt;
use harnx_core::require_nextest;
use harnx_runtime::nats_session_metadata::{
    read_invalidation_subject, SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use std::time::Duration;

fn new_remote_session_id() -> String {
    format!("convergence-session-{}", uuid::Uuid::new_v4())
}

/// Two distinct clients (separate NATS connections) converge to last-writer state.
///
/// Client A marks read, Client B marks unread; revision ordering ensures the
/// last-writer's CAS wins and the earlier write's revision is rejected.
#[tokio::test]
async fn two_clients_converge_to_last_writer_state() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    // Client A's connection and store
    let client_a = async_nats::connect(server.url()).await?;
    let jetstream_a = async_nats::jetstream::new(client_a.clone());
    let store_a = SessionMetadataStore::ensure(&jetstream_a, 1).await?;

    // Client B's connection and store (separate NATS client)
    let client_b = async_nats::connect(server.url()).await?;
    let jetstream_b = async_nats::jetstream::new(client_b.clone());
    let store_b = SessionMetadataStore::ensure(&jetstream_b, 1).await?;

    let session_id = new_remote_session_id();

    // Create session via client A
    store_a
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named("test-agent", Default::default()),
        ))
        .await?;

    // Subscribe to read-invalidation via both clients
    let mut read_sub_a = client_a
        .subscribe(read_invalidation_subject(&session_id))
        .await?;
    let mut read_sub_b = client_b
        .subscribe(read_invalidation_subject(&session_id))
        .await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Bump attention via client A (simulate worker finishing a turn)
    store_a.bump_attention(&session_id, 10).await?;

    // Wait for invalidation
    let _ = tokio::time::timeout(Duration::from_secs(5), read_sub_a.next()).await;

    // Session is now unread - verify both clients see this
    let state_a = store_a.get_read_state(&session_id).await?;
    let state_b = store_b.get_read_state(&session_id).await?;
    assert!(
        state_a.is_unread(),
        "client A should see unread after attention bump"
    );
    assert!(
        state_b.is_unread(),
        "client B should see unread after attention bump"
    );
    assert_eq!(state_a.last_attention_seq, 10);
    assert_eq!(state_b.last_attention_seq, 10);

    // Client A marks read
    store_a.mark_read(&session_id).await?;
    let _ = tokio::time::timeout(Duration::from_secs(5), read_sub_a.next()).await;

    // Verify client A sees read state
    let state_a_after = store_a.get_read_state(&session_id).await?;
    assert!(
        !state_a_after.is_unread(),
        "client A should see read after mark_read"
    );
    assert_eq!(state_a_after.last_read_seq, 10);

    // Client B marks unread (sets manual_unread flag) - this should work
    // because it's a CAS that will retry on conflict
    store_b.mark_unread(&session_id).await?;
    let _ = tokio::time::timeout(Duration::from_secs(5), read_sub_b.next()).await;

    // Both clients converge: manual_unread is set
    let state_final_a = store_a.get_read_state(&session_id).await?;
    let state_final_b = store_b.get_read_state(&session_id).await?;

    assert!(
        state_final_a.is_unread(),
        "client A should see unread after mark_unread"
    );
    assert!(
        state_final_b.is_unread(),
        "client B should see unread after mark_unread"
    );
    assert!(state_final_a.manual_unread, "client A: manual_unread set");
    assert!(state_final_b.manual_unread, "client B: manual_unread set");
    // read cursor unchanged by mark_unread
    assert_eq!(state_final_a.last_read_seq, 10);
    assert_eq!(state_final_b.last_read_seq, 10);

    // Client A marks read again - clears manual_unread and checks revision
    store_a.mark_read(&session_id).await?;
    let _ = tokio::time::timeout(Duration::from_secs(5), read_sub_a.next()).await;

    // Both converge to read state
    let state_end_a = store_a.get_read_state(&session_id).await?;
    let state_end_b = store_b.get_read_state(&session_id).await?;

    assert!(
        !state_end_a.is_unread(),
        "client A: should be read after final mark_read"
    );
    assert!(
        !state_end_b.is_unread(),
        "client B: should be read after final mark_read"
    );
    assert!(!state_end_a.manual_unread);
    assert!(!state_end_b.manual_unread);

    Ok(())
}

/// Verify that the monotonic cursor prevents a stale bool from corrupting state.
///
/// mark_unread only sets the manual_unread flag andnever moves the cursor backward.
#[tokio::test]
async fn manual_unread_does_not_corrupt_cursor() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    // Two separate clients
    let client_a = async_nats::connect(server.url()).await?;
    let jetstream_a = async_nats::jetstream::new(client_a);
    let store_a = SessionMetadataStore::ensure(&jetstream_a, 1).await?;

    let client_b = async_nats::connect(server.url()).await?;
    let jetstream_b = async_nats::jetstream::new(client_b);
    let store_b = SessionMetadataStore::ensure(&jetstream_b, 1).await?;

    let session_id = new_remote_session_id();

    // Create via client A
    store_a
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named("test-agent", Default::default()),
        ))
        .await?;

    // Attention bump via client A
    store_a.bump_attention(&session_id, 5).await?;

    // Mark read via client A
    store_a.mark_read(&session_id).await?;
    let state_a = store_a.get_read_state(&session_id).await?;
    assert_eq!(state_a.last_read_seq, 5);
    assert!(!state_a.is_unread());

    // Client B also verifies
    let state_b = store_b.get_read_state(&session_id).await?;
    assert_eq!(state_b.last_read_seq, 5);
    assert!(!state_b.is_unread());

    // Attention bump via client B (new turn)
    store_b.bump_attention(&session_id, 10).await?;
    let state_b = store_b.get_read_state(&session_id).await?;
    assert!(
        state_b.is_unread(),
        "new attention should make session unread"
    );
    assert_eq!(state_b.last_attention_seq, 10);
    assert_eq!(
        state_b.last_read_seq, 5,
        "read cursor should not advance automatically"
    );

    // Mark unread manually via client A
    store_a.mark_unread(&session_id).await?;
    let state_a = store_a.get_read_state(&session_id).await?;
    assert!(state_a.is_unread());
    assert!(state_a.manual_unread);
    assert_eq!(state_a.last_attention_seq, 10);
    assert_eq!(state_a.last_read_seq, 5, "read cursor unchanged");

    // Client B verifies convergence
    let state_b = store_b.get_read_state(&session_id).await?;
    assert!(state_b.is_unread());
    assert!(state_b.manual_unread);

    // Mark read clears manual_unread and advances cursor
    store_b.mark_read(&session_id).await?;
    let state = store_a.get_read_state(&session_id).await?;
    assert!(!state.is_unread());
    assert!(!state.manual_unread);
    assert_eq!(
        state.last_read_seq, 10,
        "read cursor advances to latest attention"
    );

    Ok(())
}

/// CAS retry ensures concurrent mark_read operations all succeed.
#[tokio::test]
async fn concurrent_mark_read_operations_succeed_via_cas_retry() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    // Create multiple separate clients
    let clients: Vec<_> = (0..5).map(|_| async_nats::connect(server.url())).collect();
    let clients: Vec<_> = futures_util::future::try_join_all(clients).await?;

    let stores: Vec<_> = futures_util::future::try_join_all(clients.iter().map(|c| async {
        let js = async_nats::jetstream::new(c.clone());
        SessionMetadataStore::ensure(&js, 1).await
    }))
    .await?;

    let session_id = new_remote_session_id();

    // Create session via first store
    stores[0]
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named("test-agent", Default::default()),
        ))
        .await?;

    // Initial attention bump
    stores[0].bump_attention(&session_id, 5).await?;

    // Concurrent mark_read operations from all clients
    let handles: Vec<_> = stores
        .iter()
        .map(|store| {
            let store = store.clone();
            let session_id = session_id.clone();
            tokio::spawn(async move { store.mark_read(&session_id).await })
        })
        .collect();

    // Wait for all operations
    for handle in handles {
        handle.await??;
    }

    // Final state should be consistent: read
    for store in &stores {
        let state = store.get_read_state(&session_id).await?;
        assert!(!state.is_unread());
        assert!(!state.manual_unread);
    }

    Ok(())
}

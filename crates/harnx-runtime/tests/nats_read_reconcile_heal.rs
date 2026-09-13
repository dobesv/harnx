//! Integration tests for client cache reconciliation and missed-invalidation heal.
//!
//! Tests that a dropped read-invalidation converges via:
//! - Periodic reconcile (refetch)
//! - Reconnect re-snapshot
//! - Log-based repair for lost worker bumps

mod common;

use anyhow::Result;
use common::spawn_nats_server;
use futures_util::StreamExt;
use harnx_core::{message::MessageContent, require_nextest, session::SessionLogEntry};
use harnx_runtime::{
    nats_session_log::NatsSessionLog,
    nats_session_metadata::{
        read_invalidation_subject, SessionInitializer, SessionMetadata, SessionMetadataStore,
        SessionReadState,
    },
    nats_worker::{derive_attention_seq, new_remote_session_id, NatsSessionLogBackend},
};
use std::time::Duration;

/// Client cache misses an invalidation, then converges via refetch.
///
/// Simulates a client that maintains a local cache and misses an invalidation
/// (subscription drop, network issue, or race). The client's periodic reconcile
/// path refetches from KV and converges to the canonical state.
#[tokio::test]
async fn missed_invalidation_heals_via_refetch() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    // Client A: the "observer" who maintains a local cache
    let client_a = async_nats::connect(server.url()).await?;
    let jetstream_a = async_nats::jetstream::new(client_a.clone());
    let store_a = SessionMetadataStore::ensure(&jetstream_a, 1).await?;

    // Client B: another client that makes modifications
    let client_b = async_nats::connect(server.url()).await?;
    let jetstream_b = async_nats::jetstream::new(client_b);
    let store_b = SessionMetadataStore::ensure(&jetstream_b, 1).await?;

    let session_id = new_remote_session_id();

    // Create session via client A
    store_a
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named("test-agent", Default::default()),
        ))
        .await?;

    // Client A subscribes to read-invalidation
    let mut read_sub_a = client_a
        .subscribe(read_invalidation_subject(&session_id))
        .await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Bump attention via client A, cache it locally
    store_a.bump_attention(&session_id, 10).await?;
    let cached_state: SessionReadState = {
        // Wait for invalidation to arrive
        let _ = tokio::time::timeout(Duration::from_secs(2), read_sub_a.next()).await;
        // Simulate client caching the state
        store_a.get_read_state(&session_id).await?
    };
    assert!(cached_state.is_unread());

    // Client B marks read - this publishes an invalidation
    store_b.mark_read(&session_id).await?;

    // Simulate missed invalidation: drop the subscription message
    // (In real scenarios: network hiccup, subscription lag, etc.)
    // We deliberately don't wait for or read the invalidation

    // Client A's periodic reconcile refetches the state
    let state_after_reconcile = store_a.get_read_state(&session_id).await?;
    assert!(
        !state_after_reconcile.is_unread(),
        "refetch should converge to canonical state after missed invalidation"
    );
    assert_eq!(state_after_reconcile.last_read_seq, 10);

    // The invalidation was still published (we can drain it)
    let _ = tokio::time::timeout(Duration::from_millis(500), read_sub_a.next()).await;

    Ok(())
}

/// Multiple invalidations missed, then reconnect/refetch converges.
///
/// On reconnect, clients re-snapshot their session list, converging to
/// the current canonical state even if multiple mutations were missed.
#[tokio::test]
async fn multiple_missed_invalidations_heal_on_reconnect() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    // Client A: observer with a cache
    let client_a = async_nats::connect(server.url()).await?;
    let jetstream_a = async_nats::jetstream::new(client_a.clone());
    let store_a = SessionMetadataStore::ensure(&jetstream_a, 1).await?;

    // Client B: modifier
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

    // Subscribe before initial snapshot
    let mut read_sub_a = client_a
        .subscribe(read_invalidation_subject(&session_id))
        .await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Initial state from Client A's perspective
    store_a.bump_attention(&session_id, 5).await?;
    store_a.mark_read(&session_id).await?;
    let _ = tokio::time::timeout(Duration::from_millis(100), read_sub_a.next()).await;

    let cached_a = store_a.get_read_state(&session_id).await?;
    assert!(!cached_a.is_unread());

    // Simulate network disconnect: drop subscription
    drop(read_sub_a);

    // Multiple mutations while Client A is "disconnected"
    store_b.mark_unread(&session_id).await?;
    store_b.bump_attention(&session_id, 10).await?;
    // mark_read clears manual_unread and advances last_read_seq
    store_b.mark_read(&session_id).await?;

    // Simulate reconnect: create new subscription and re-snapshot
    let mut read_sub_a_reconnected = client_a
        .subscribe(read_invalidation_subject(&session_id))
        .await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Re-snapshot after reconnect
    let state_after_reconnect = store_a.get_read_state(&session_id).await?;
    assert!(
        !state_after_reconnect.is_unread(),
        "reconnected client should see mark_read result"
    );
    // mark_read clears manual_unread
    assert!(!state_after_reconnect.manual_unread);
    assert_eq!(state_after_reconnect.last_attention_seq, 10);
    assert_eq!(
        state_after_reconnect.last_read_seq, 10,
        "read cursor should be at 10 after mark_read"
    );

    // Client A receives subsequent invalidations properly
    store_b.bump_attention(&session_id, 20).await?;
    let _ = tokio::time::timeout(Duration::from_secs(2), read_sub_a_reconnected.next()).await;

    let state_final = store_a.get_read_state(&session_id).await?;
    assert!(state_final.is_unread());
    assert_eq!(state_final.last_attention_seq, 20);

    Ok(())
}

/// Lost attention bump (worker crash before KV write) repairs via reconcile_attention_from_log.
///
/// This tests the server-side repair path. When a worker crashes after appending
/// a TurnEnd but before bumping attention (or CAS fails), the log contains the
/// attention-producing entry. `reconcile_attention_from_log` derives the attention
/// sequence from the log and repairs the KV entry.
#[tokio::test]
async fn lost_attention_bump_heals_via_log_reconciliation() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;

    let session_id = format!("lost-bump-{}", uuid::Uuid::new_v4());

    // Create session
    store
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named("test-agent", Default::default()),
        ))
        .await?;

    // Create backend with metadata store attached (for reconcile_attention_from_log)
    let backend = NatsSessionLogBackend::new(jetstream.clone(), &session_id)
        .with_metadata_store(Some(store.clone()));

    // Append log entries WITHOUT bumping attention (simulate crash before KV write)
    let log = NatsSessionLog::new(jetstream.clone(), session_id.clone());
    log.append_event_async(&SessionLogEntry::Message {
        id: None,
        role: harnx_core::message::MessageRole::User,
        content: MessageContent::Text("test prompt".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;

    // Append TurnEnd with through_seq > 0 (should have bumped attention but didn't)
    let seq = log
        .append_event_async(&SessionLogEntry::TurnEnd {
            through_seq: 1,
            fence_token: 1,
            timestamp: None,
            usage: None,
        })
        .await?;

    // Verify attention NOT bumped in KV (simulating lost bump)
    let state_before = store.get_read_state(&session_id).await?;
    assert!(
        !state_before.is_unread(),
        "should NOT be unread before reconciliation (lost bump)"
    );
    assert_eq!(state_before.last_attention_seq, 0);

    // Load log entries
    let entries: Vec<(u64, SessionLogEntry)> = backend.load_events_latest_async().await?;

    // Derive attention seq from log
    let attention_seq = derive_attention_seq(&entries);
    assert_eq!(
        attention_seq, seq,
        "derive_attention_seq should find TurnEnd seq"
    );

    // Reconcile via the backend method (server-side repair)
    backend.reconcile_attention_from_log(&entries).await?;

    // Verify repair
    let state_after = store.get_read_state(&session_id).await?;
    assert!(
        state_after.is_unread(),
        "session should be unread after reconciliation repaired attention"
    );
    assert_eq!(state_after.last_attention_seq, seq);

    Ok(())
}

/// Cross-client activity converges via reconcile.
///
/// Two clients with independent caches perform mutations and converge to the
/// last-writer state. This tests that CAS ordering prevents stale state from
/// overwriting newer state, and demonstrates subscribe-before-snapshot semantics.
#[tokio::test]
async fn cross_client_activity_converges_on_reconcile() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    // Client A
    let client_a = async_nats::connect(server.url()).await?;
    let jetstream_a = async_nats::jetstream::new(client_a.clone());
    let store_a = SessionMetadataStore::ensure(&jetstream_a, 1).await?;

    // Client B
    let client_b = async_nats::connect(server.url()).await?;
    let jetstream_b = async_nats::jetstream::new(client_b.clone());
    let store_b = SessionMetadataStore::ensure(&jetstream_b, 1).await?;

    let session_id = new_remote_session_id();

    // Create via client A
    store_a
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named("test-agent", Default::default()),
        ))
        .await?;

    // Subscribe-before-snapshot: both subscribe before reading state
    let mut read_sub_a = client_a
        .subscribe(read_invalidation_subject(&session_id))
        .await?;
    let mut read_sub_b = client_b
        .subscribe(read_invalidation_subject(&session_id))
        .await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Initial state: Client A bumps attention
    store_a.bump_attention(&session_id, 5).await?;
    // Wait for both to receive invalidation
    let _ = tokio::time::timeout(Duration::from_millis(100), read_sub_a.next()).await;
    let _ = tokio::time::timeout(Duration::from_millis(100), read_sub_b.next()).await;

    // Take initial snapshot (simulating list fetch)
    let mut cached_a = store_a.get_read_state(&session_id).await?;
    let mut cached_b = store_b.get_read_state(&session_id).await?;
    assert!(cached_a.is_unread());
    assert!(cached_b.is_unread());

    // Rapid cross-client mutations
    // Client A marks read
    store_a.mark_read(&session_id).await?;
    let _ = tokio::time::timeout(Duration::from_millis(100), read_sub_a.next()).await;
    let _ = tokio::time::timeout(Duration::from_millis(100), read_sub_b.next()).await;

    // Both refetch
    cached_a = store_a.get_read_state(&session_id).await?;
    cached_b = store_b.get_read_state(&session_id).await?;
    assert!(!cached_a.is_unread());
    assert!(!cached_b.is_unread());

    // Client B marks unread (stale client might have stale cache)
    store_b.mark_unread(&session_id).await?;
    let _ = tokio::time::timeout(Duration::from_millis(100), read_sub_a.next()).await;
    let _ = tokio::time::timeout(Duration::from_millis(100), read_sub_b.next()).await;

    // Client B bumps attention (simulates worker activity)
    store_b.bump_attention(&session_id, 10).await?;
    let _ = tokio::time::timeout(Duration::from_millis(100), read_sub_a.next()).await;
    let _ = tokio::time::timeout(Duration::from_millis(100), read_sub_b.next()).await;

    // Both reconcile (refetch on interval/reconnect)
    let state_a = store_a.get_read_state(&session_id).await?;
    let state_b = store_b.get_read_state(&session_id).await?;

    // Both converge to last-writer state
    assert!(
        state_a.is_unread(),
        "final state: unread due to attention > read"
    );
    assert!(
        state_b.is_unread(),
        "final state: unread due to attention > read"
    );
    assert_eq!(state_a.last_attention_seq, 10);
    assert_eq!(state_b.last_attention_seq, 10);
    assert_eq!(state_a.last_read_seq, 5); // mark_read advanced it to 5
    assert_eq!(state_b.last_read_seq, 5);
    assert!(state_a.manual_unread); // still set from mark_unread
    assert!(state_b.manual_unread);

    Ok(())
}

/// CAS ordering prevents stale bool from overwriting newer state.
///
/// Demonstrates that even with race conditions, the monotonic cursor model
/// and CAS semantics prevent stale state from corrupting the canonical value.
#[tokio::test]
async fn two_clients_converge_to_last_writer_state() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    // Client A
    let client_a = async_nats::connect(server.url()).await?;
    let jetstream_a = async_nats::jetstream::new(client_a.clone());
    let store_a = SessionMetadataStore::ensure(&jetstream_a, 1).await?;

    // Client B (different connection)
    let client_b = async_nats::connect(server.url()).await?;
    let jetstream_b = async_nats::jetstream::new(client_b.clone());
    let store_b = SessionMetadataStore::ensure(&jetstream_b, 1).await?;

    let session_id = format!("two-clients-{}", uuid::Uuid::new_v4());

    // Create session
    store_a
        .create(&SessionMetadata::new(
            &session_id,
            SessionInitializer::named("test-agent", Default::default()),
        ))
        .await?;

    // Subscribe
    let mut read_sub_a = client_a
        .subscribe(read_invalidation_subject(&session_id))
        .await?;
    let mut read_sub_b = client_b
        .subscribe(read_invalidation_subject(&session_id))
        .await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Initial attention bump
    store_a.bump_attention(&session_id, 10).await?;
    let _ = tokio::time::timeout(Duration::from_millis(100), read_sub_a.next()).await;
    let _ = tokio::time::timeout(Duration::from_millis(100), read_sub_b.next()).await;

    // Get current state revision
    let state_a1 = store_a.get_read_state(&session_id).await?;
    let state_b1 = store_b.get_read_state(&session_id).await?;
    assert_eq!(state_a1.last_attention_seq, 10);
    assert_eq!(state_b1.last_attention_seq, 10);

    // Client A marks read - this advances last_read_seq and clears manual_unread
    store_a.mark_read(&session_id).await?;
    let state_a2 = store_a.get_read_state(&session_id).await?;
    assert!(!state_a2.is_unread());
    assert_eq!(state_a2.last_read_seq, 10);

    // Client B tries to mark unread - but CAS ensures monotonicity
    store_b.mark_unread(&session_id).await?;

    // Client B bumps attention to a higher sequence
    store_b.bump_attention(&session_id, 20).await?;

    // Wait for invalidations
    let _ = tokio::time::timeout(Duration::from_millis(100), read_sub_a.next()).await;
    let _ = tokio::time::timeout(Duration::from_millis(100), read_sub_b.next()).await;

    // Both clients see the convergent state
    let state_a3 = store_a.get_read_state(&session_id).await?;
    let state_b3 = store_b.get_read_state(&session_id).await?;

    // Both should see unread (attention=20 > read=10, plus manual_unread)
    assert!(state_a3.is_unread());
    assert!(state_b3.is_unread());

    // last_read_seq should NOT have moved backward
    assert!(state_a3.last_read_seq >= 10);
    assert!(state_b3.last_read_seq >= 10);

    // Both should see the same canonical state
    assert_eq!(state_a3.last_attention_seq, state_b3.last_attention_seq);
    assert_eq!(state_a3.last_read_seq, state_b3.last_read_seq);
    assert_eq!(state_a3.manual_unread, state_b3.manual_unread);

    Ok(())
}

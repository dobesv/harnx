//! Integration tests for worker attention sequence derivation and repair.
//!
//! Tests the attention derivation logic (`derive_attention_seq`) and the repair
//! path via `reconcile_attention_from_log`.
//!
//! ## What these tests do NOT cover
//!
//! These tests do **not** test `record_session_turn_end` or the HITL approval
//! request callback directly. Those code paths are tested internally in:
//! - `daemon_session_exec::attention_tests::record_session_turn_end_bumps_attention_directly`
//! - `agent_loop::hitl_attention_tests::hitl_approval_callback_bumps_attention_directly`
//!
//! ## What these tests cover
//!
//! - `derive_attention_seq` for deriving attention seq from log entries
//! - `reconcile_attention_from_log` for repairing lost bumps

mod common;
#[path = "common/generation.rs"]
mod generation;

use anyhow::Result;
use common::spawn_nats_server;
use harnx_core::{
    message::{MessageContent, MessageRole},
    require_nextest,
    session::SessionLogEntry,
};
use harnx_runtime::{
    nats_session_log::NatsSessionLog,
    nats_session_metadata::{SessionInitializer, SessionMetadata, SessionMetadataStore},
    nats_worker::{derive_attention_seq, new_remote_session_id, NatsSessionLogBackend},
};

// ---------------------------------------------------------------------------
// Test helpers for attention entry construction
// ---------------------------------------------------------------------------

/// Create a user message log entry.
fn user_message_entry(content: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text(content.to_string()),
        timestamp: None,
        fence_token: None,
    }
}

/// Create an assistant message log entry.
fn assistant_message_entry(content: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::Assistant,
        content: MessageContent::Text(content.to_string()),
        timestamp: None,
        fence_token: None,
    }
}

/// Create a TurnEnd log entry with the given through_seq.
fn turn_end_entry(through_seq: u64) -> SessionLogEntry {
    SessionLogEntry::TurnEnd {
        through_seq,
        fence_token: 1,
        timestamp: None,
        usage: None,
    }
}

/// Create a HitlApprovalRequested log entry.
fn hitl_approval_entry(tool_call_id: &str, summary: &str) -> SessionLogEntry {
    SessionLogEntry::HitlApprovalRequested {
        tool_call_id: tool_call_id.to_string(),
        summary: summary.to_string(),
        fence_token: 1,
    }
}

/// Setup helper: create store with session metadata for attention tests.
async fn setup_attention_test_store(
    server: &common::NatsServerHandle,
    agent: &str,
) -> Result<(SessionMetadataStore, String)> {
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let session_id = new_remote_session_id();
    let metadata = SessionMetadata::new(
        &session_id,
        SessionInitializer::named(agent, Default::default()),
    );
    let storage_key = metadata.storage_key();
    store.create(&metadata).await?;

    Ok((store, storage_key))
}

/// Setup helper: create store with session metadata and backend for attention tests.
async fn setup_attention_test(
    server: &common::NatsServerHandle,
    agent: &str,
) -> Result<(SessionMetadataStore, String, NatsSessionLogBackend)> {
    let (store, session_id) = setup_attention_test_store(server, agent).await?;
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let backend = generation::fenced_backend(&jetstream, &session_id)
        .await?
        .with_metadata_store(Some(store.clone()));
    Ok((store, session_id, backend))
}

/// Test that TurnEnd append via `reconcile_attention_from_log` repairs lost bump.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turn_end_bumps_attention_via_backend() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let (store, session_id, backend) = setup_attention_test(&server, "metis").await?;

    // Append some messages to have a valid through_seq
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let log = NatsSessionLog::new(jetstream.clone(), session_id.clone());
    log.append_event_async(&user_message_entry("test prompt"))
        .await?;
    log.append_event_async(&assistant_message_entry("test reply"))
        .await?;

    // Append TurnEnd directly to log (without bump), then reconcile
    log.append_event_async(&turn_end_entry(2)).await?;

    // Verify session is NOT unread before reconcile
    let state = store.get_read_state(&session_id).await?;
    assert!(
        !state.is_unread(),
        "session should NOT be unread before reconcile"
    );

    // Reconcile to repair the lost bump
    let entries = backend.load_events_latest_async().await?;
    backend.reconcile_attention_from_log(&entries).await?;

    // Verify session is now unread after reconciliation
    let state = store.get_read_state(&session_id).await?;
    assert!(
        state.is_unread(),
        "session should be unread after reconcile (lost-bump repair for TurnEnd)"
    );

    Ok(())
}

/// Test that HITL approval request via `reconcile_attention_from_log` repairs lost bump.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hitl_approval_requested_bumps_attention_via_backend() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let (store, session_id, backend) = setup_attention_test(&server, "metis").await?;

    // Append a HitlApprovalRequested without any bump (simulating lost bump)
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let log = NatsSessionLog::new(jetstream.clone(), session_id.clone());
    log.append_event_async(&SessionLogEntry::HitlApprovalRequested {
        tool_call_id: "test-call-123".to_string(),
        summary: "Approve this".to_string(),
        fence_token: 1,
    })
    .await?;

    // Verify session is NOT unread before reconcile (no bump was done)
    let state = store.get_read_state(&session_id).await?;
    assert!(
        !state.is_unread(),
        "session should NOT be unread before reconcile (lost bump scenario)"
    );

    // Reconcile
    let entries = backend.load_events_latest_async().await?;
    backend.reconcile_attention_from_log(&entries).await?;

    // Verify session is now unread after reconciliation
    let state = store.get_read_state(&session_id).await?;
    assert!(
        state.is_unread(),
        "session should be unread after reconcile (lost-bump repair for HitlApprovalRequested)"
    );
    assert!(state.last_attention_seq >= 1);

    Ok(())
}

/// Test that reconcile_attention_from_log repairs a lost bump.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconcile_attention_from_log_repairs_lost_bump() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let (store, session_id, backend) = setup_attention_test(&server, "metis").await?;

    // Append a TurnEnd without any bump (simulating lost bump - append directly to log)
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let log = NatsSessionLog::new(jetstream.clone(), session_id.clone());
    log.append_event_async(&turn_end_entry(1)).await?;

    // Verify session is NOT unread before reconcile (no bump was done)
    let state = store.get_read_state(&session_id).await?;
    assert!(
        !state.is_unread(),
        "session should NOT be unread before reconcile (lost bump scenario)"
    );

    // Load entries and call reconcile
    let entries = backend.load_events_latest_async().await?;
    backend.reconcile_attention_from_log(&entries).await?;

    // Verify session is now unread after reconciliation
    let state = store.get_read_state(&session_id).await?;
    assert!(
        state.is_unread(),
        "session should be unread after reconcile (lost-bump repair)"
    );

    Ok(())
}

/// Non-attention entry does not bump attention.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_attention_entry_does_not_bump() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let (store, session_id, backend) = setup_attention_test(&server, "metis").await?;

    // Append a regular message - this should NOT bump attention
    backend
        .append_event(&SessionLogEntry::Message {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text("test prompt".to_string()),
            timestamp: None,
            fence_token: None,
        })
        .await?;

    // Reconcile (should do nothing)
    backend
        .reconcile_attention_from_log(&backend.load_events_latest_async().await?)
        .await?;

    // Verify session is NOT unread (attention seq should be 0)
    let state = store.get_read_state(&session_id).await?;
    assert!(
        !state.is_unread(),
        "session should NOT be unread after non-attention entry"
    );
    assert_eq!(state.last_attention_seq, 0);

    Ok(())
}

/// Restart persistence: state read back after new store handle == unread.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attention_seq_persists_across_store_handles() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let (_store1, session_id, backend) = setup_attention_test(&server, "metis").await?;

    // Append a final TurnEnd
    let seq = backend
        .append_event(&SessionLogEntry::TurnEnd {
            through_seq: 1,
            fence_token: 1,
            timestamp: None,
            usage: None,
        })
        .await?;

    // Reconcile to bump attention
    backend
        .reconcile_attention_from_log(&backend.load_events_latest_async().await?)
        .await?;

    // Create a new store handle (simulating restart)
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store2 = SessionMetadataStore::ensure(&jetstream, 1).await?;

    // Verify state persists with new handle
    let state = store2.get_read_state(&session_id).await?;
    assert!(
        state.is_unread(),
        "session should still be unread after new store handle"
    );
    assert_eq!(state.last_attention_seq, seq);

    Ok(())
}

/// Lost-bump repair: derive_attention_seq finds TurnEnd.
#[test]
fn derive_attention_seq_finds_turn_end() {
    let entries = vec![(1, user_message_entry("prompt")), (2, turn_end_entry(1))];

    let attention_seq = derive_attention_seq(&entries);
    assert_eq!(
        attention_seq, 2,
        "derive_attention_seq should return seq of TurnEnd"
    );
}

/// Lost-bump repair: derive_attention_seq finds HitlApprovalRequested.
#[test]
fn derive_attention_seq_finds_hitl_approval_requested() {
    let entries = vec![(5, hitl_approval_entry("call-123", "Approve this"))];

    let attention_seq = derive_attention_seq(&entries);
    assert_eq!(
        attention_seq, 5,
        "derive_attention_seq should return seq of HitlApprovalRequested"
    );
}

/// Lost-bump repair: derive_attention_seq ignores non-attention entries.
#[test]
fn derive_attention_seq_ignores_non_attention_entries() {
    let entries = vec![
        (1, user_message_entry("prompt")),
        (2, assistant_message_entry("reply")),
    ];

    let attention_seq = derive_attention_seq(&entries);
    assert_eq!(
        attention_seq, 0,
        "derive_attention_seq should return 0 for non-attention entries"
    );
}

/// Lost-bump repair: derive_attention_seq ignores TurnEnd with through_seq == 0.
#[test]
fn derive_attention_seq_ignores_zero_through_seq() {
    let entries = vec![(10, turn_end_entry(0))];

    let attention_seq = derive_attention_seq(&entries);
    assert_eq!(
        attention_seq, 0,
        "derive_attention_seq should ignore TurnEnd with through_seq == 0"
    );
}

/// Lost-bump repair: derive_attention_seq returns max of attention entries.
#[test]
fn derive_attention_seq_returns_max() {
    let entries = vec![
        (1, turn_end_entry(1)),
        (5, hitl_approval_entry("call-123", "Approve this")),
        (3, turn_end_entry(2)),
    ];

    let attention_seq = derive_attention_seq(&entries);
    assert_eq!(
        attention_seq, 5,
        "derive_attention_seq should return max attention seq"
    );
}

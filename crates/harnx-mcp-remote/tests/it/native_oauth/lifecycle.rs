#![cfg(test)]
//! Integration tests for OAuth lifecycle: restarts, invalid_grant recovery, 401 retries, and in-flight races.

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{json, Value};

use super::*;
use crate::native_common::*;

/// Restarting server republishes registrations from the stored catalog without upstream calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_republishes_after_refresh() {
    harnx_core::require_nextest();
    let mock = MockOAuthServer::spawn(3600).await.expect("spawn mock");
    let shared = shared_nats(&mock.url, &[]).await;
    let first = start_instance(&shared).await;

    complete_instance_connect(&first, "alice").await;

    // Shorten access expiry and trigger proactive refresh
    shorten_access_expiry_to_near_expiry(&first.server.lifecycle, &alice_principal()).await;
    let _ = invoke(
        &first.server.toolset,
        "whoami",
        Some("alice"),
        &[],
        Value::Null,
    )
    .await
    .expect("whoami");
    let initial_mcp_requests = mock.state.mcp_request_count.load(Ordering::SeqCst);

    // Stop first instance and remove registration with retry on in-flight background CAS conflict
    let stored = stored_registration(&first.server.lifecycle.handle, &audience("alice"))
        .await
        .expect("registration");
    let conn_id = stored.registration.connection_id.clone().expect("conn");
    first.shutdown.cancel();

    delete_registration_with_retry(&first.server.lifecycle.handle, &audience("alice"), &conn_id)
        .await;

    // Start second instance on same shared NATS
    let second = start_instance(&shared).await;
    let recovered = wait_for_stored_registration(
        &second.server.lifecycle.handle,
        &audience("alice"),
        Duration::from_secs(15),
    )
    .await;

    assert_eq!(
        recovered.registration.connection_id.as_deref(),
        Some(conn_id.as_str())
    );
    let pub_tools: std::collections::HashSet<&str> = recovered
        .registration
        .tools
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    assert!(
        pub_tools.contains("whoami"),
        "republished registration retains whoami tool"
    );
    assert!(
        pub_tools.contains("search"),
        "republished registration retains search tool"
    );
    assert_eq!(
        pub_tools.len(),
        2,
        "republished registration retains exactly whoami and search"
    );
    assert_eq!(
        mock.state.mcp_request_count.load(Ordering::SeqCst),
        initial_mcp_requests,
        "republish made no upstream call"
    );

    second.shutdown.cancel();
    mock.shutdown().await;
}

/// invalid_grant on refresh marks status needs_login and cleans up user catalog and registration.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_grant_marks_needs_login_and_cleans_up() {
    harnx_core::require_nextest();
    let fixture = OAuthFixture::spawn(3600).await;
    let toolset = fixture.toolset();
    let lifecycle = fixture.lifecycle();

    fixture.connect_alice().await;

    assert!(stored_registration(&lifecycle.handle, &audience("alice"))
        .await
        .is_some());

    // Configure mock OAuth server to fail refresh with invalid_grant
    fixture
        .mock
        .state
        .invalid_grant
        .store(true, Ordering::SeqCst);

    // Shorten access expiry to +10s
    shorten_access_expiry_to_near_expiry(lifecycle, &alice_principal()).await;

    // Upstream tool call triggers refresh which fails with invalid_grant
    let error = invoke(toolset, "whoami", Some("alice"), &[], Value::Null)
        .await
        .expect_err("fails on invalid_grant");
    let ToolInvokeError::Recoverable(msg) = error else {
        panic!("expected recoverable error, got {error:?}");
    };
    assert!(
        msg.contains("jira credential expired; call jira_connect"),
        "{msg}"
    );

    // Registration and catalog are cleaned up
    assert!(stored_registration(&lifecycle.handle, &audience("alice"))
        .await
        .is_none());

    // Status tool reports needs_login
    let status_res = invoke(
        toolset,
        "credential_status",
        Some("alice"),
        &[],
        Value::Null,
    )
    .await
    .expect("status");
    assert_eq!(status_res["status"], json!("needs_login"));

    fixture.shutdown().await;
}

/// Upstream 401 triggers token refresh and transparent single retry; 403 yields safe insufficient_scope error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_401_retries_with_refresh_and_403_insufficient_scope() {
    harnx_core::require_nextest();
    let fixture = OAuthFixture::spawn(3600).await;
    let toolset = fixture.toolset();

    fixture.connect_alice().await;

    // 1. Simulate 401 on the active access token
    fixture
        .mock
        .state
        .rejected_tokens
        .lock()
        .unwrap()
        .insert(fixture.mock.state.initial_access_token.clone());

    // Calling whoami receives 401, refreshes token, and retries successfully
    let who = invoke(toolset, "whoami", Some("alice"), &[], Value::Null)
        .await
        .expect("whoami after 401 retry");
    assert_eq!(who["content"][0]["text"], json!("user:alice@example.test"));
    assert_eq!(fixture.mock.state.refresh_count.load(Ordering::SeqCst), 1);

    // 2. Simulate 403 insufficient_scope
    fixture
        .mock
        .state
        .simulate_403
        .store(true, Ordering::SeqCst);
    let error = invoke(toolset, "whoami", Some("alice"), &[], Value::Null)
        .await
        .expect_err("403 error");
    let ToolInvokeError::Recoverable(msg) = error else {
        panic!("expected recoverable error, got {error:?}");
    };
    assert!(
        msg.contains("missing required scope (insufficient_scope)"),
        "{msg}"
    );

    fixture.shutdown().await;
}

/// Transient OAuth refresh error followed by recovery allows subsequent invoke to succeed
/// after closed pool peer transport is evicted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_refresh_error_recovers_on_subsequent_invoke() {
    harnx_core::require_nextest();
    let fixture = OAuthFixture::spawn(3600).await;
    let toolset = fixture.toolset();
    let lifecycle = fixture.lifecycle();

    fixture.connect_alice().await;

    // Shorten access expiry to +10s
    shorten_access_expiry_to_near_expiry(lifecycle, &alice_principal()).await;

    // Inject transient 500 error on next refresh
    fixture
        .mock
        .state
        .transient_refresh_error
        .store(true, Ordering::SeqCst);

    // Near-expiry call triggers refresh which fails transiently
    let err = invoke(toolset, "whoami", Some("alice"), &[], Value::Null)
        .await
        .expect_err("transient refresh error");
    assert!(matches!(err, ToolInvokeError::Recoverable(_)));

    // Provider has now recovered (transient_refresh_error swapped to false)
    // Subsequent invoke evicts closed peer transport and succeeds
    let who = invoke(toolset, "whoami", Some("alice"), &[], Value::Null)
        .await
        .expect("whoami after recovery");
    assert_eq!(who["content"][0]["text"], json!("user:alice@example.test"));

    fixture.shutdown().await;
}

/// Deterministic in-flight refresh race:
/// While refresh token exchange is in flight, a reconnect replaces the credential with connection 2.
/// The stale refresh commit CAS fails and does NOT overwrite connection 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deterministic_refresh_replaced_network_in_flight() {
    harnx_core::require_nextest();
    let fixture = OAuthFixture::spawn(3600).await;
    let toolset = fixture.toolset();
    let lifecycle = fixture.lifecycle();

    // Connect connection 1
    fixture.connect_alice().await;

    let conn1_id = lifecycle
        .credentials
        .for_principal(alice_principal())
        .unwrap()
        .get()
        .await
        .unwrap()
        .unwrap()
        .record
        .connection_id;

    // Shorten access expiry to +10s
    shorten_access_expiry_to_near_expiry(lifecycle, &alice_principal()).await;

    // Set up pause channel in mock server for next refresh
    let (pause_tx, pause_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    *fixture.mock.state.refresh_pause_tx.lock().unwrap() = Some(pause_tx);
    *fixture.mock.state.refresh_resume_rx.lock().unwrap() = Some(resume_rx);

    // Spawn task that calls whoami (which starts near-expiry refresh)
    let toolset_clone = RemoteCredentialsToolset::new(
        fixture.instance.server.lifecycle.clone(),
        fixture.instance.server.toolset.base_url.clone(),
    );
    let refresh_task = tokio::spawn(async move {
        invoke(&toolset_clone, "whoami", Some("alice"), &[], Value::Null).await
    });

    // Wait until refresh is in flight at mock server
    pause_rx.await.expect("refresh paused in flight");

    // While refresh is in flight, Alice reconnects!
    let (challenge_id2, round2) = start_connect_flow(toolset, "alice").await;
    let (s2, _) = drive_oauth_connect(&fixture.instance.http_base, &challenge_id2, &[])
        .await
        .expect("c2");
    assert_eq!(s2, 200);
    let _ = continue_connect(toolset, "alice", &challenge_id2, round2).await;

    let conn2_id = lifecycle
        .credentials
        .for_principal(alice_principal())
        .unwrap()
        .get()
        .await
        .unwrap()
        .unwrap()
        .record
        .connection_id;
    assert_ne!(conn1_id, conn2_id);

    // Now resume the in-flight refresh of connection 1
    let _ = resume_tx.send(());

    // Refresh task completes (it sees credential changed or was superseded)
    let _ = refresh_task.await;

    // Authoritative credential in NATS is STILL connection 2!
    let current = lifecycle
        .credentials
        .for_principal(alice_principal())
        .unwrap()
        .get()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        current.record.connection_id, conn2_id,
        "connection 2 was not clobbered by stale refresh"
    );

    fixture.shutdown().await;
}

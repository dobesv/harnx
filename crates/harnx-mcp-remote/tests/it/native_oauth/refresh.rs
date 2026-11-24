#![cfg(test)]
//! Integration tests for proactive, concurrent, and rotated OAuth token refreshes.

use std::sync::atomic::Ordering;

use serde_json::{json, Value};

use super::*;
use crate::native_common::*;

/// Near-expiry proactive refresh:
/// An access token expiring in 10s is refreshed before the upstream tool call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn near_expiry_proactive_refresh() {
    harnx_core::require_nextest();
    let fixture = OAuthFixture::spawn(3600).await;
    fixture.connect_alice().await;

    assert_eq!(fixture.mock.state.refresh_count.load(Ordering::SeqCst), 0);

    // Shorten access_expires_at to +10s to trigger <= 60s near-expiry threshold
    shorten_access_expiry_to_near_expiry(fixture.lifecycle(), &alice_principal()).await;

    // Calling whoami triggers proactive refresh
    let who = invoke(fixture.toolset(), "whoami", Some("alice"), &[], Value::Null)
        .await
        .expect("whoami");
    assert_eq!(who["content"][0]["text"], json!("user:alice@example.test"));

    // Exactly 1 refresh occurred
    assert_eq!(fixture.mock.state.refresh_count.load(Ordering::SeqCst), 1);

    fixture.shutdown().await;
}

/// Concurrent tool calls across two instances trigger exactly one refresh under NATS lease coordination.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_calls_trigger_single_refresh() {
    harnx_core::require_nextest();
    let fixture = OAuthFixture::spawn(3600).await;
    let peer = fixture.spawn_peer().await;
    fixture.connect_alice().await;

    assert_eq!(fixture.mock.state.refresh_count.load(Ordering::SeqCst), 0);

    // Shorten access_expires_at to +10s
    shorten_access_expiry_to_near_expiry(fixture.lifecycle(), &alice_principal()).await;

    // Run two concurrent tool invocations across two separate server instances sharing NATS
    let (res1, res2) = tokio::join!(
        invoke(fixture.toolset(), "whoami", Some("alice"), &[], Value::Null),
        invoke(
            &peer.server.toolset,
            "search",
            Some("alice"),
            &[],
            Value::Null
        ),
    );

    assert_eq!(
        res1.expect("whoami")["content"][0]["text"],
        json!("user:alice@example.test")
    );
    assert_eq!(
        res2.expect("search")["content"][0]["text"],
        json!("search:alice@example.test")
    );

    // Only 1 refresh request was made upstream thanks to NATS lease coordination
    assert_eq!(fixture.mock.state.refresh_count.load(Ordering::SeqCst), 1);

    peer.shutdown.cancel();
    fixture.shutdown().await;
}

/// Token rotation persists new refresh token in NATS and preserves connection_id and expires_at,
/// with catalog continuity maintained.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_rotation_preserves_connection_id_and_expiry_and_persists() {
    harnx_core::require_nextest();
    let fixture = OAuthFixture::spawn(3600).await;
    let lifecycle = fixture.lifecycle();
    let toolset = fixture.toolset();

    fixture.connect_alice().await;

    let initial = lifecycle
        .credentials
        .for_principal(alice_principal())
        .expect("store")
        .get()
        .await
        .expect("credential get")
        .expect("credential present");
    let initial_conn = initial.record.connection_id.clone();
    let initial_expires_at = initial.record.expires_at;

    // Shorten access_expires_at to +10s
    shorten_access_expiry_to_near_expiry(lifecycle, &alice_principal()).await;

    // Trigger refresh via tool call
    let who = invoke(toolset, "whoami", Some("alice"), &[], Value::Null)
        .await
        .expect("whoami");
    assert_eq!(who["content"][0]["text"], json!("user:alice@example.test"));
    assert_eq!(fixture.mock.state.refresh_count.load(Ordering::SeqCst), 1);

    // Read updated credential using store.get()
    let updated = lifecycle
        .credentials
        .for_principal(alice_principal())
        .expect("store")
        .get()
        .await
        .expect("credential get")
        .expect("credential present");

    // connection_id and expires_at are preserved
    assert_eq!(updated.record.connection_id, initial_conn);
    assert_eq!(updated.record.expires_at, initial_expires_at);

    // Refresh token was rotated and persisted
    if let CredentialMaterial::Oauth2(ref mat) = updated.record.material {
        let rt = mat.refresh_token.as_ref().expect("rotated refresh token");
        assert!(rt.expose().contains("refresh-rot-1"));
    } else {
        panic!("expected oauth2 material");
    }

    // Catalog and registration still carry initial connection_id
    let registration = stored_registration(&lifecycle.handle, &audience("alice"))
        .await
        .expect("registration");
    assert_eq!(
        registration.registration.connection_id.as_deref(),
        Some(initial_conn.as_str())
    );

    let catalog_snapshot = lifecycle
        .catalogs
        .for_principal(alice_principal())
        .expect("catalog store")
        .inspect()
        .await
        .expect("catalog snapshot");
    let catalog = catalog_snapshot.record.expect("catalog record");
    assert_eq!(
        catalog.connection_id, initial_conn,
        "catalog connection_id maintained across refresh"
    );
    assert_eq!(catalog.tools.len(), 2, "catalog tools intact");

    let reg_names: std::collections::HashSet<&str> = registration
        .registration
        .tools
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    let cat_names: std::collections::HashSet<&str> =
        catalog.tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(
        reg_names, cat_names,
        "catalog and registration tool names match"
    );
    assert!(reg_names.contains("whoami"), "registration retains whoami");
    assert!(reg_names.contains("search"), "registration retains search");

    fixture.shutdown().await;
}

/// A refresh occurring between browser completion and Continue still allows Continue to succeed,
/// executing actual auth_factory refresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_between_browser_completion_and_continue() {
    harnx_core::require_nextest();
    let fixture = OAuthFixture::spawn(3600).await;
    let toolset = fixture.toolset();
    let lifecycle = fixture.lifecycle();

    let (challenge_id, round) = start_connect_flow(toolset, "alice").await;

    let (status, _body) = drive_oauth_connect(&fixture.instance.http_base, &challenge_id, &[])
        .await
        .expect("connect");
    assert_eq!(status, 200);

    // Before Continue is submitted, shorten access expiry and trigger actual refresh through auth_factory
    shorten_access_expiry_to_near_expiry(lifecycle, &alice_principal()).await;
    let store = lifecycle
        .credentials
        .for_principal(alice_principal())
        .expect("store");
    let versioned = store.get().await.expect("get cred").expect("cred present");
    let auth = lifecycle
        .auth_factory
        .build(&versioned)
        .expect("build auth");
    auth.ensure_fresh()
        .await
        .expect("run real auth_factory refresh");

    assert_eq!(fixture.mock.state.refresh_count.load(Ordering::SeqCst), 1);

    // Alice now submits Continue in harnx
    let result = continue_connect(toolset, "alice", &challenge_id, round).await;

    assert!(
        result["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("Connected as alice@example.test; 2 jira tools are available"),
        "{result}"
    );

    fixture.shutdown().await;
}

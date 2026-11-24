#![cfg(test)]

use std::sync::atomic::Ordering;
use std::time::Duration;

use harnx_mcp_remote::native::pool::ListChanged;
use harnx_toolset_server::ConditionalDelete;

use super::fixture::*;
use crate::native_common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_changed_updates_the_user_catalog_and_registration() {
    harnx_core::require_nextest();
    let (_keys, fixture, upstream) = standard_fixture().await;
    let lifecycle = fixture.server.lifecycle.clone();
    connect_user(
        &fixture.server.toolset,
        &fixture.http_base,
        Some("alice"),
        ALICE_TOKEN,
    )
    .await
    .expect("connect alice");
    let connection_id = credential_of(&lifecycle, &alice_principal())
        .await
        .record
        .connection_id;

    // The upstream adds a tool for Alice and notifies. The SSE notification
    // drives the pool handler; the same signal is also sent directly so the
    // test does not depend on notification delivery timing.
    upstream.users.set_tools(
        ALICE_TOKEN,
        vec!["whoami".into(), "search".into(), "report".into()],
    );
    upstream.notify_list_changed().await;
    lifecycle
        .pool
        .notify_sender()
        .send(ListChanged {
            principal: alice_principal(),
            connection_id: connection_id.clone(),
        })
        .await
        .expect("pool signal");

    let catalog = wait_for_catalog_tool(&lifecycle, &alice_principal(), "report").await;
    assert_eq!(catalog.record.connection_id, connection_id);
    let registration = wait_for_registration_tool(&lifecycle, &audience("alice"), "report").await;
    assert!(
        registration
            .registration
            .tools
            .iter()
            .any(|tool| tool.name == "search"),
        "the previous tools are kept: {:?}",
        registration
            .registration
            .tools
            .iter()
            .map(|tool| &tool.name)
            .collect::<Vec<_>>()
    );
}

async fn connect_alice_instance(shared: &SharedNats) -> Instance {
    let instance = start_instance(shared).await;
    connect_user(
        &instance.server.toolset,
        &instance.http_base,
        Some("alice"),
        ALICE_TOKEN,
    )
    .await
    .expect("connect alice");
    instance
}

/// Restarting an instance republishes Alice's registration from the stored
/// catalog, with no upstream call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_republishes_without_upstream_calls() {
    harnx_core::require_nextest();
    let (shared, upstream) = multi_instance_fixture(&[]).await;
    let first = connect_alice_instance(&shared).await;
    let initializations = upstream.initialize_count.load(Ordering::SeqCst);
    assert!(initializations >= 1);
    let connection_id = credential_of(&first.server.lifecycle, &alice_principal())
        .await
        .record
        .connection_id;

    // The first instance stops and its registration is removed (TTL or an
    // operator delete); a second instance must recover it from the catalog.
    let stored = stored_registration(&first.server.lifecycle.handle, &audience("alice"))
        .await
        .expect("registration after connect");
    first.shutdown.cancel();
    let stored_connection = stored
        .registration
        .connection_id
        .as_deref()
        .expect("connection");
    let deleted = delete_registration_checked(
        &first.server.lifecycle.handle,
        &audience("alice"),
        stored_connection,
        stored_connection,
    )
    .await;
    assert_eq!(deleted, ConditionalDelete::Deleted);
    assert!(
        stored_registration(&first.server.lifecycle.handle, &audience("alice"))
            .await
            .is_none()
    );

    let second = start_instance(&shared).await;
    let recovered = {
        let handle = &second.server.lifecycle.handle;
        let user_audience = audience("alice");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(stored) = stored_registration(handle, &user_audience).await {
                break stored;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out waiting for the recovered registration");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    assert_eq!(
        recovered
            .registration
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["whoami", "search"],
        "the catalog is republished as it was"
    );
    assert_eq!(
        recovered.registration.connection_id.as_deref(),
        Some(connection_id.as_str()),
        "recovery keeps the connection id"
    );
    assert_eq!(
        upstream.initialize_count.load(Ordering::SeqCst),
        initializations,
        "startup recovery makes no upstream call"
    );

    first.shutdown.cancel();
    second.shutdown.cancel();
}

/// A stale instance (still refreshing for an old connection) must not touch a
/// newer connection's registration.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_instance_refresh_tick_cannot_overwrite_a_newer_registration() {
    harnx_core::require_nextest();
    let (shared, upstream) = multi_instance_fixture(&[]).await;
    let instance_a = connect_alice_instance(&shared).await;
    let first_connection = credential_of(&instance_a.server.lifecycle, &alice_principal())
        .await
        .record
        .connection_id;

    // A reconnect through B: conn2, catalog and registration now belong to
    // B. The upstream gained a tool before the reconnect so the two
    // connections' derived state differs.
    upstream.users.set_tools(
        ALICE_TOKEN,
        vec!["whoami".into(), "search".into(), "report".into()],
    );
    let instance_b = start_instance(&shared).await;
    connect_user(
        &instance_b.server.toolset,
        &instance_b.http_base,
        Some("alice"),
        ALICE_TOKEN,
    )
    .await
    .expect("reconnect alice on B");
    let second_connection = credential_of(&instance_b.server.lifecycle, &alice_principal())
        .await
        .record
        .connection_id;
    assert_ne!(first_connection, second_connection);

    // Stop B so nothing else writes; capture the live registration.
    instance_b.shutdown.cancel();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = stored_registration(&instance_b.server.lifecycle.handle, &audience("alice"))
        .await
        .expect("registration belongs to the new connection");
    assert_eq!(
        before.registration.connection_id.as_deref(),
        Some(second_connection.as_str())
    );

    // A drops its old ownership, then may adopt the current connection from
    // the authoritative credential and matching catalog. Refreshing that
    // connection must preserve B's payload.
    for _ in 0..3 {
        instance_a
            .server
            .lifecycle
            .refresh_tick()
            .await
            .expect("tick");
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after = stored_registration(&instance_a.server.lifecycle.handle, &audience("alice"))
        .await
        .expect("registration intact");
    assert!(after.revision >= before.revision);
    assert_eq!(
        serde_json::to_value(&after.registration).expect("after payload"),
        serde_json::to_value(&before.registration).expect("before payload"),
        "adoption refreshes the current payload, never the stale payload"
    );
    assert_eq!(
        after.registration.connection_id.as_deref(),
        Some(second_connection.as_str()),
        "a stale tick cannot replace the newer connection"
    );
    assert!(
        after
            .registration
            .tools
            .iter()
            .any(|tool| tool.name == "report"),
        "the newer connection's tools are untouched"
    );

    instance_a.shutdown.cancel();
}

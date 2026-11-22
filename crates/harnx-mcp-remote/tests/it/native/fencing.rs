#![cfg(test)]

use std::time::Duration;

use harnx_toolset::ToolInvokeError;
use harnx_toolset_server::ConditionalDelete;
use serde_json::{json, Value};

use super::fixture::*;
use crate::native_common::*;

async fn drive_cross_instance_connect(
    instance_a: &Instance,
    instance_b: &Instance,
) -> (String, u64) {
    let (challenge, challenge_id, round) =
        start_connect_flow(&instance_a.server.toolset, Some("alice")).await;
    assert!(
        challenge
            .url
            .as_deref()
            .unwrap_or_default()
            .starts_with(&instance_a.http_base),
        "A created the link: {:?}",
        challenge.url
    );
    let (status, body) =
        complete_token_form(&instance_b.http_base, &challenge_id, ALICE_TOKEN, None, &[])
            .await
            .expect("complete through B");
    assert_eq!(status, 200, "{body}");
    (challenge_id, round)
}

async fn verify_alice_connected(
    instance_a: &Instance,
    instance_b: &Instance,
    challenge_id: &str,
    round: u64,
) {
    let result = invoke(
        &instance_a.server.toolset,
        "connect",
        Some("alice"),
        &[(connect_challenge_key(round), continue_response())],
        json!({ "challenge_id": challenge_id, "round": round }),
    )
    .await
    .expect("continue on A");
    assert!(
        result["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("Connected; 2 jira tools"),
        "{result}"
    );
    let who = invoke(
        &instance_a.server.toolset,
        "whoami",
        Some("alice"),
        &[],
        Value::Null,
    )
    .await
    .expect("whoami through A");
    assert_eq!(
        who["content"][0]["text"],
        json!(format!("user:{ALICE_LABEL}"))
    );
    assert!(
        stored_registration(&instance_b.server.lifecycle.handle, &audience("alice"))
            .await
            .is_some(),
        "the registration is visible on both instances"
    );
}

/// Two instances sharing JetStream state: a flow created on one completes on the
/// other, and a disconnect on one is not undone by the other's refresh loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_instances_share_flows_and_fence_disconnect() {
    harnx_core::require_nextest();
    let (shared, _upstream) = multi_instance_fixture(&[]).await;
    let instance_a = start_instance(&shared).await;
    let instance_b = start_instance(&shared).await;

    let (challenge_id, round) = drive_cross_instance_connect(&instance_a, &instance_b).await;
    verify_alice_connected(&instance_a, &instance_b, &challenge_id, round).await;

    // Disconnect on A. B (the managing instance) must not resurrect it.
    let revision = credential_of(&instance_b.server.lifecycle, &alice_principal())
        .await
        .revision;
    invoke(
        &instance_a.server.toolset,
        "disconnect",
        Some("alice"),
        &[],
        Value::Null,
    )
    .await
    .expect("disconnect on A");
    assert!(
        stored_registration(&instance_b.server.lifecycle.handle, &audience("alice"))
            .await
            .is_none(),
        "the disconnect removed the shared registration"
    );

    // B's refresh tick drops its bookkeeping instead of recreating, and B's
    // background loop keeps it deleted across several 100 ms ticks.
    instance_b
        .server
        .lifecycle
        .refresh_tick()
        .await
        .expect("tick");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        stored_registration(&instance_b.server.lifecycle.handle, &audience("alice"))
            .await
            .is_none(),
        "a disconnect elsewhere is never undone by the refresh loop"
    );
    let gone = invoke(
        &instance_b.server.toolset,
        "whoami",
        Some("alice"),
        &[],
        Value::Null,
    )
    .await
    .expect_err("credential gone on B too");
    assert!(matches!(gone, ToolInvokeError::Recoverable(_)));

    // A stale credential write at the pre-disconnect revision fails: the KV
    // revision CAS is the guard.
    let store = instance_b
        .server
        .lifecycle
        .credentials
        .for_principal(alice_principal())
        .expect("store");
    let stale = store
        .put_connect(revision, token_input("resurrection-token"))
        .await;
    assert!(matches!(stale, Err(harnx_credentials::Error::Conflict)));

    instance_a.shutdown.cancel();
    instance_b.shutdown.cancel();
}

/// Deterministic fencing: a delayed writer from an old connection can neither
/// replace nor delete the newer connection's derived state, and the new
/// connection keeps working end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_connection_derived_state_cannot_replace_newer_state() {
    harnx_core::require_nextest();
    let (_keys, fixture, _upstream) = standard_fixture().await;
    let toolset = &fixture.server.toolset;
    let lifecycle = fixture.server.lifecycle.clone();

    // First connection for Alice.
    connect_user(toolset, &fixture.http_base, Some("alice"), ALICE_TOKEN)
        .await
        .expect("first connect");
    let first_snapshot = lifecycle
        .credentials
        .for_principal(alice_principal())
        .expect("store")
        .inspect()
        .await
        .expect("snapshot");
    let first_connection = first_snapshot
        .record
        .as_ref()
        .expect("record")
        .connection_id
        .clone();

    // A reconnect replaces the credential with a new connection id.
    connect_user(toolset, &fixture.http_base, Some("alice"), ALICE_TOKEN)
        .await
        .expect("reconnect");
    let second = credential_of(&lifecycle, &alice_principal()).await;
    let second_connection = second.record.connection_id.clone();
    assert_ne!(first_connection, second_connection);

    // A delayed list_changed from the old connection cannot update the new
    // catalog (fencing: the stored connection id has moved on).
    lifecycle
        .apply_list_changed(alice_principal(), first_connection.clone())
        .await;
    let catalogs = lifecycle
        .catalogs
        .for_principal(alice_principal())
        .expect("catalogs");
    let catalog = catalogs.get().await.expect("catalog").expect("present");
    assert_eq!(catalog.record.connection_id, second_connection);

    // A stale catalog write under the old connection id is refused outright.
    let fresh_revision = catalogs.inspect().await.expect("snapshot").revision;
    let stale_write = catalogs
        .update(
            fresh_revision,
            &first_connection,
            vec![rmcp::model::Tool::new(
                "stale",
                "stale",
                std::sync::Arc::new(serde_json::Map::new()),
            )],
        )
        .await;
    assert!(
        matches!(
            stale_write,
            Err(harnx_credentials::Error::ConnectionMismatch)
        ),
        "a stale catalog write is refused"
    );

    // A disconnect from the old connection's era leaves the new catalog and
    // registration in place (conditional deletes).
    assert!(!catalogs
        .delete_if_connection(&first_connection)
        .await
        .expect("delete"));
    assert!(
        catalogs
            .get()
            .await
            .expect("catalog")
            .expect("kept")
            .record
            .connection_id
            == second_connection
    );
    let outcome = delete_registration_checked(
        &lifecycle.handle,
        &audience("alice"),
        &second_connection,
        &first_connection,
    )
    .await;
    assert_eq!(outcome, ConditionalDelete::ConnectionMismatch);
    assert!(
        stored_registration(&lifecycle.handle, &audience("alice"))
            .await
            .is_some(),
        "the newer registration survives"
    );

    // A stale credential delete is refused; the new credential stays.
    let stale_delete = lifecycle
        .credentials
        .for_principal(alice_principal())
        .expect("store")
        .delete(first_snapshot.revision)
        .await;
    assert_eq!(stale_delete, Err(harnx_credentials::Error::Conflict));

    // Alice's new connection keeps working end to end.
    let who = invoke(toolset, "whoami", Some("alice"), &[], Value::Null)
        .await
        .expect("whoami through the new connection");
    assert_eq!(
        who["content"][0]["text"],
        json!(format!("user:{ALICE_LABEL}"))
    );
}

/// A flow whose credential is superseded before its catalog write fails with
/// `superseded by a newer connection` and cannot clobber the newer state.
/// The interleaving (claim -> credential write -> another reconnect wins ->
/// delayed catalog write) is driven through the lifecycle methods the
/// completing route uses, in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_superseded_flow_cannot_clobber_a_newer_connection() {
    harnx_core::require_nextest();
    let (_keys, fixture, _upstream) = standard_fixture().await;
    let toolset = &fixture.server.toolset;
    let lifecycle = fixture.server.lifecycle.clone();
    let principal = alice_principal();

    connect_user(toolset, &fixture.http_base, Some("alice"), ALICE_TOKEN)
        .await
        .expect("baseline connect");

    // Flow F starts; the POST route's first steps run: claim, then the
    // credential write (conn F).
    let (_challenge, challenge_id, round) = start_connect_flow(toolset, Some("alice")).await;
    let pending = lifecycle
        .flows
        .get_by_challenge(&challenge_id)
        .await
        .expect("flow read")
        .expect("flow present");
    let claimed = lifecycle
        .flows
        .claim(&challenge_id, pending.revision)
        .await
        .expect("claim");
    let superseded_credential = lifecycle
        .connect_credential(&principal, token_input(ALICE_TOKEN))
        .await
        .expect("credential write");

    // While F is between its credential and catalog writes, another
    // reconnect completes fully and wins.
    connect_user(toolset, &fixture.http_base, Some("alice"), ALICE_TOKEN)
        .await
        .expect("the winning reconnect");
    let winner = credential_of(&lifecycle, &principal).await;
    assert_ne!(
        winner.record.connection_id,
        superseded_credential.record.connection_id
    );

    // F's delayed catalog/registration write is fenced out.
    let error = lifecycle
        .finish_connection(&principal, &challenge_id, &superseded_credential)
        .await
        .expect_err("the superseded flow fails");
    assert!(
        error
            .to_string()
            .contains("superseded by a newer connection"),
        "{error:#}"
    );

    // The route marks the flow failed with that message (decision 7 step 3),
    // and Continue reports it instead of success.
    lifecycle
        .flows
        .fail(
            &challenge_id,
            claimed.revision,
            "superseded by a newer connection".into(),
        )
        .await
        .expect("flow failed");
    let reported = invoke(
        toolset,
        "connect",
        Some("alice"),
        &[(connect_challenge_key(round), continue_response())],
        json!({ "challenge_id": challenge_id, "round": round }),
    )
    .await
    .expect_err("failed flow reported");
    let ToolInvokeError::Recoverable(message) = reported else {
        panic!("expected recoverable, got {reported:?}")
    };
    assert!(
        message.contains("superseded by a newer connection"),
        "{message}"
    );
    assert!(message.contains("jira_connect"), "{message}");

    // The winner's derived state is untouched.
    let catalog = lifecycle
        .catalogs
        .for_principal(principal.clone())
        .expect("catalogs")
        .get()
        .await
        .expect("catalog")
        .expect("present");
    assert_eq!(catalog.record.connection_id, winner.record.connection_id);
    let registration = stored_registration(&lifecycle.handle, &audience("alice"))
        .await
        .expect("registration");
    assert_eq!(
        registration.registration.connection_id.as_deref(),
        Some(winner.record.connection_id.as_str())
    );
}

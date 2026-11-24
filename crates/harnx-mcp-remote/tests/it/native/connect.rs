#![cfg(test)]

use std::sync::atomic::Ordering;

use harnx_toolset::ToolInvokeError;
use harnx_toolset_server::HandleAudience;
use serde_json::{json, Value};

use super::fixture::*;
use crate::native_common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alice_connects_sees_her_tools_and_bob_never_does() {
    harnx_core::require_nextest();
    let (_keys, fixture, upstream) = standard_fixture().await;
    let server = &fixture.server;
    let toolset = &server.toolset;
    let lifecycle = &server.lifecycle;

    let result = connect_user(
        &fixture.server.toolset,
        &fixture.http_base,
        Some("alice"),
        ALICE_TOKEN,
    )
    .await
    .expect("connect alice");
    let text = result["content"][0]["text"].as_str().expect("text result");
    // Token flows carry no account label (the upstream only identifies the
    // user through tool results), so the message is the plain form.
    assert!(
        text.contains("Connected; 2 jira tools"),
        "connected message: {text}"
    );
    assert!(!text.contains(ALICE_TOKEN), "no secret in the result");

    // Alice's registration carries her tool list under her audience; Bob's
    // audience has none.
    let alice_registration = stored_registration(&lifecycle.handle, &audience("alice"))
        .await
        .expect("alice registration published");
    assert_eq!(
        alice_registration
            .registration
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["whoami", "search"],
        "reserved names excluded, upstream order kept"
    );
    assert_eq!(
        alice_registration.registration.user_id.as_deref(),
        Some("alice")
    );
    assert!(
        alice_registration
            .registration
            .tools
            .iter()
            .all(|tool| tool.requests_input()),
        "MRTR-capable upstream tools keep the durable challenge path"
    );
    assert!(
        stored_registration(&lifecycle.handle, &audience("bob"))
            .await
            .is_none(),
        "bob has no registration"
    );

    // Alice's upstream call reaches the upstream with her token (the upstream
    // answers with her label, never with the token itself).
    let who = invoke(toolset, "whoami", Some("alice"), &[], Value::Null)
        .await
        .expect("alice whoami");
    assert_eq!(
        who["content"][0]["text"],
        json!(format!("user:{ALICE_LABEL}"))
    );
    assert!(
        !serde_json::to_string(&who)
            .expect("serialize")
            .contains(ALICE_TOKEN),
        "no secret in the tool result"
    );

    // A forged request from Bob for Alice's tool never reaches upstream.
    let forged = invoke(toolset, "whoami", Some("bob"), &[], Value::Null)
        .await
        .expect_err("bob has no credential");
    match forged {
        ToolInvokeError::Recoverable(message) => {
            assert!(message.contains("not connected"), "{message}");
        }
        other => panic!("expected a recoverable error, got {other:?}"),
    }
    assert_eq!(upstream.initialize_count.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_removes_credential_catalog_and_registration() {
    harnx_core::require_nextest();
    let (_keys, fixture, _upstream) = standard_fixture().await;
    let toolset = &fixture.server.toolset;
    let lifecycle = fixture.server.lifecycle.clone();
    connect_user(
        &fixture.server.toolset,
        &fixture.http_base,
        Some("alice"),
        ALICE_TOKEN,
    )
    .await
    .expect("connect alice");

    let status = invoke(
        toolset,
        "credential_status",
        Some("alice"),
        &[],
        Value::Null,
    )
    .await
    .expect("status");
    assert_eq!(status["connected"], json!(true));
    assert_eq!(status["status"], json!("active"));
    assert!(
        status["expires_at"].as_str().is_some(),
        "no secrets: {status}"
    );

    invoke(toolset, "disconnect", Some("alice"), &[], Value::Null)
        .await
        .expect("disconnect");
    assert!(stored_registration(&lifecycle.handle, &audience("alice"))
        .await
        .is_none());
    let catalogs = lifecycle
        .catalogs
        .for_principal(alice_principal())
        .expect("catalog store");
    assert!(catalogs.get().await.expect("catalog read").is_none());

    let status = invoke(
        toolset,
        "credential_status",
        Some("alice"),
        &[],
        Value::Null,
    )
    .await
    .expect("status");
    assert_eq!(status["connected"], json!(false));

    let gone = invoke(toolset, "whoami", Some("alice"), &[], Value::Null)
        .await
        .expect_err("credential deleted");
    assert!(matches!(gone, ToolInvokeError::Recoverable(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continue_reissues_challenge_and_cancel_marks_flow_cancelled() {
    harnx_core::require_nextest();
    let (_keys, fixture, _upstream) = standard_fixture().await;
    let toolset = &fixture.server.toolset;

    let error = invoke(toolset, "connect", Some("alice"), &[], Value::Null)
        .await
        .expect_err("challenge");
    let (challenge, state) = url_challenge(error);
    let challenge_id = state["challenge_id"].as_str().expect("id").to_string();
    let round = state["round"].as_u64().expect("round");

    // Continue while the flow is still pending re-issues the same link under
    // a new round key (an unfinished reconnect behaves the same).
    let reissued = invoke(
        toolset,
        "connect",
        Some("alice"),
        &[(connect_challenge_key(round), continue_response())],
        json!({ "challenge_id": challenge_id, "round": round }),
    )
    .await
    .expect_err("still pending re-issues");
    let (challenge2, state2) = url_challenge(reissued);
    assert_eq!(state2["round"], json!(round + 1), "next round");
    assert_eq!(challenge2.url, challenge.url, "same link re-issued");
    let round2 = state2["round"].as_u64().expect("round");

    // Cancel on a still-pending flow reports cancellation.
    let cancelled = invoke(
        toolset,
        "connect",
        Some("alice"),
        &[(connect_challenge_key(round2), cancel_response())],
        json!({ "challenge_id": challenge_id, "round": round2 }),
    )
    .await
    .expect("cancel result");
    assert_eq!(
        cancelled["content"][0]["text"],
        json!("connection cancelled")
    );

    // The cancelled link no longer completes: the page and POST both refuse.
    let (status, body) =
        complete_token_form(&fixture.http_base, &challenge_id, ALICE_TOKEN, None, &[])
            .await
            .expect("token post after cancel");
    assert_eq!(status, 409, "{body}");

    // Cancel after completion says so instead of cancelling.
    connect_user(
        &fixture.server.toolset,
        &fixture.http_base,
        Some("alice"),
        ALICE_TOKEN,
    )
    .await
    .expect("connect alice");
    let error = invoke(toolset, "connect", Some("alice"), &[], Value::Null)
        .await
        .expect_err("second connect challenge");
    let (_, state) = url_challenge(error);
    let challenge_id = state["challenge_id"].as_str().expect("id").to_string();
    let round = state["round"].as_u64().expect("round");
    let (status, body) =
        complete_token_form(&fixture.http_base, &challenge_id, ALICE_TOKEN, None, &[])
            .await
            .expect("complete");
    assert_eq!(status, 200, "{body}");
    let cancelled = invoke(
        toolset,
        "connect",
        Some("alice"),
        &[(connect_challenge_key(round), cancel_response())],
        json!({ "challenge_id": challenge_id, "round": round }),
    )
    .await
    .expect("cancel after complete");
    assert_eq!(
        cancelled["content"][0]["text"],
        json!("connection already completed; call jira_connect again to reconnect")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_user_mode_serves_ownerless_only() {
    harnx_core::require_nextest();
    let upstream = spawn_per_user_upstream("authorization", false)
        .await
        .expect("upstream");
    let keys = tempfile::TempDir::new().expect("temp dir");
    let key_file = write_key_file(&keys);
    let cli = native_cli(&upstream.url, &key_file, &["--single-user"]);
    let fixture = start_native(config_from_cli(&cli)).await.expect("native");
    let toolset = &fixture.server.toolset;
    let lifecycle = fixture.server.lifecycle.clone();

    // An identified request is rejected before reaching upstream.
    let identified = invoke(toolset, "whoami", Some("alice"), &[], Value::Null)
        .await
        .expect_err("identified request");
    let ToolInvokeError::Recoverable(message) = identified else {
        panic!("expected recoverable, got {identified:?}")
    };
    assert!(message.contains("single-user mode"), "{message}");

    // The ownerless session connects and sees its tools as an ownerless-only
    // registration.
    let result = connect_user(
        &fixture.server.toolset,
        &fixture.http_base,
        None,
        OWNERLESS_TOKEN,
    )
    .await
    .expect("ownerless connect");
    assert!(
        result["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("2 jira tools"),
        "{result}"
    );
    let ownerless = stored_registration(&lifecycle.handle, &HandleAudience::OwnerlessOnly)
        .await
        .expect("ownerless registration");
    assert!(ownerless.registration.ownerless_only);
    assert!(ownerless.registration.user_id.is_none());
    assert!(stored_registration(&lifecycle.handle, &audience("alice"))
        .await
        .is_none());

    // The ownerless upstream call works; Alice still cannot use it.
    let who = invoke(toolset, "whoami", None, &[], Value::Null)
        .await
        .expect("ownerless whoami");
    assert_eq!(
        who["content"][0]["text"],
        json!(format!("user:{OWNERLESS_LABEL}"))
    );
    let rejected = invoke(toolset, "whoami", Some("alice"), &[], Value::Null)
        .await
        .expect_err("identified rejected");
    assert!(matches!(rejected, ToolInvokeError::Recoverable(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_header_and_scheme_apply_to_the_upstream() {
    harnx_core::require_nextest();
    // A Pendo-style integration key: raw token in a custom header, no scheme.
    let upstream = spawn_per_user_upstream("x-pendo-integration-key", false)
        .await
        .expect("upstream");
    let keys = tempfile::TempDir::new().expect("temp dir");
    let key_file = write_key_file(&keys);
    let cli = native_cli(
        &upstream.url,
        &key_file,
        &[
            "--token-header",
            "x-pendo-integration-key",
            "--token-scheme",
            "",
        ],
    );
    let fixture = start_native(config_from_cli(&cli)).await.expect("native");

    connect_user(
        &fixture.server.toolset,
        &fixture.http_base,
        Some("alice"),
        ALICE_TOKEN,
    )
    .await
    .expect("connect alice");
    let who = invoke(
        &fixture.server.toolset,
        "whoami",
        Some("alice"),
        &[],
        Value::Null,
    )
    .await
    .expect("whoami with the raw key");
    assert_eq!(
        who["content"][0]["text"],
        json!(format!("user:{ALICE_LABEL}"))
    );
}

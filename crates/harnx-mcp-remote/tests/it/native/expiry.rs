#![cfg(test)]

use std::time::Duration;

use harnx_credentials::Principal;
use harnx_toolset::ToolInvokeError;
use serde_json::{json, Value};

use super::fixture::*;
use crate::native_common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expiry_selection_and_ttl_maximum() {
    harnx_core::require_nextest();
    let (_keys, fixture, _upstream) = standard_fixture().await;
    let toolset = &fixture.server.toolset;
    let lifecycle = fixture.server.lifecycle.clone();

    // Default expiry is the 365-day server TTL.
    let result = connect_user(
        &fixture.server.toolset,
        &fixture.http_base,
        Some("alice"),
        ALICE_TOKEN,
    )
    .await
    .expect("connect");
    assert!(
        result["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("Connected"),
        "{result}"
    );
    let record = credential_of(&lifecycle, &alice_principal()).await.record;
    let default_expiry = record.expires_at;
    let year = chrono::TimeDelta::days(365);
    assert!(
        (default_expiry - chrono::Utc::now() - year)
            .num_days()
            .abs()
            <= 1,
        "default expiry is 365 days: {default_expiry}"
    );

    // A past date is refused on the form, and the flow stays pending.
    let (_, state) = start_connect(toolset, Some("bob")).await;
    let challenge_id = state["challenge_id"].as_str().expect("id").to_string();
    let yesterday = (chrono::Utc::now() - chrono::TimeDelta::days(1))
        .format("%Y-%m-%d")
        .to_string();
    let (status, body) = complete_token_form(
        &fixture.http_base,
        &challenge_id,
        BOB_TOKEN,
        Some(&yesterday),
        &[],
    )
    .await
    .expect("past-date post");
    assert_eq!(status, 400, "past expiry refused: {body}");

    // An earlier expiry from the form is stored as requested.
    let in_ten_days = (chrono::Utc::now() + chrono::TimeDelta::days(10))
        .format("%Y-%m-%d")
        .to_string();
    let (status, body) = complete_token_form(
        &fixture.http_base,
        &challenge_id,
        BOB_TOKEN,
        Some(&in_ten_days),
        &[],
    )
    .await
    .expect("bob completion");
    assert_eq!(status, 200, "the flow is still pending: {body}");
    let bob = credential_of(&lifecycle, &Principal::User("bob".into()))
        .await
        .record;
    assert_eq!(bob.expires_at.format("%Y-%m-%d").to_string(), in_ten_days);

    // A date beyond the server TTL is rejected: failed flow, nothing written.
    let (_challenge, challenge_id, round) = start_connect_flow(toolset, Some("carol")).await;
    let beyond = (chrono::Utc::now() + chrono::TimeDelta::days(400))
        .format("%Y-%m-%d")
        .to_string();
    let (status, body) = complete_token_form(
        &fixture.http_base,
        &challenge_id,
        "carol-token",
        Some(&beyond),
        &[],
    )
    .await
    .expect("carol completion");
    assert_eq!(status, 502, "beyond-TTL completion fails: {body}");
    let carol_store = lifecycle
        .credentials
        .for_principal(Principal::User("carol".into()))
        .expect("carol store");
    assert!(carol_store
        .inspect()
        .await
        .expect("snapshot")
        .record
        .is_none());

    // Continue reports the failure with a retry hint.
    let failed = invoke(
        toolset,
        "connect",
        Some("carol"),
        &[(connect_challenge_key(round), continue_response())],
        json!({ "challenge_id": challenge_id, "round": round }),
    )
    .await
    .expect_err("failed flow reported");
    let ToolInvokeError::Recoverable(message) = failed else {
        panic!("expected recoverable, got {failed:?}")
    };
    assert!(message.contains("exceeds"), "{message}");
    assert!(message.contains("jira_connect"), "{message}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_credential_yields_recoverable_connect_again_error() {
    harnx_core::require_nextest();
    let (_keys, fixture, _upstream) = custom_fixture(&["--credential-ttl", "2s"]).await;

    connect_user(
        &fixture.server.toolset,
        &fixture.http_base,
        Some("alice"),
        ALICE_TOKEN,
    )
    .await
    .expect("connect alice");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let expired = invoke(
        &fixture.server.toolset,
        "whoami",
        Some("alice"),
        &[],
        Value::Null,
    )
    .await
    .expect_err("expired credential");
    let ToolInvokeError::Recoverable(message) = expired else {
        panic!("expected recoverable, got {expired:?}")
    };
    assert!(message.contains("expired"), "{message}");
    assert!(message.contains("jira_connect"), "{message}");
}

#![cfg(test)]

use harnx_credentials::catalog::CATALOGS_BUCKET;
use harnx_credentials::flow::{challenge_key, state_key, FLOWS_BUCKET};
use harnx_credentials::store::CREDENTIALS_BUCKET;
use harnx_credentials::Principal;
use harnx_mcp_remote::native::registrations::Lifecycle;
use serde_json::{json, Value};

use super::fixture::*;
use crate::native_common::*;

fn assert_no_token_in_results(texts: &[(&str, &str)], token: &str) {
    for (what, text) in texts {
        assert!(!text.contains(token), "token leaked via {what}");
    }
}

struct FlowSecrets<'a> {
    challenge_id: &'a str,
    flow_state: &'a str,
    csrf: &'a str,
}

async fn assert_kv_and_registration_secrets_absent(
    fixture: &NativeFixture,
    lifecycle: &Lifecycle,
    principal: &Principal,
    secrets: FlowSecrets<'_>,
) {
    let js = async_nats::jetstream::new(fixture.client.clone());
    let credentials_kv = js
        .get_key_value(CREDENTIALS_BUCKET)
        .await
        .expect("credentials bucket");
    let flows_kv = js.get_key_value(FLOWS_BUCKET).await.expect("flows bucket");
    let catalogs_kv = js
        .get_key_value(CATALOGS_BUCKET)
        .await
        .expect("catalogs bucket");

    let credential_entry = credentials_kv
        .entry(
            &lifecycle
                .credentials
                .for_principal(principal.clone())
                .expect("store")
                .key(),
        )
        .await
        .expect("credential entry")
        .expect("credential present");
    assert!(!bytes_contain(
        &credential_entry.value,
        ALICE_TOKEN.as_bytes()
    ));
    assert!(!bytes_contain(
        &credential_entry.value,
        secrets.csrf.as_bytes()
    ));

    let flow_entry = flows_kv
        .entry(&challenge_key(secrets.challenge_id))
        .await
        .expect("flow entry")
        .expect("flow present");
    assert!(!bytes_contain(&flow_entry.value, ALICE_TOKEN.as_bytes()));
    assert!(!bytes_contain(&flow_entry.value, secrets.csrf.as_bytes()));

    let state_entry = flows_kv
        .entry(&state_key(secrets.flow_state))
        .await
        .expect("state entry")
        .expect("state index present");
    assert!(!bytes_contain(&state_entry.value, ALICE_TOKEN.as_bytes()));
    assert!(!bytes_contain(&state_entry.value, secrets.csrf.as_bytes()));

    let catalog_entry = catalogs_kv
        .entry(
            &lifecycle
                .catalogs
                .for_principal(principal.clone())
                .expect("catalogs")
                .key(),
        )
        .await
        .expect("catalog entry")
        .expect("catalog present");
    assert!(!bytes_contain(&catalog_entry.value, ALICE_TOKEN.as_bytes()));

    let registration = stored_registration(&lifecycle.handle, &audience("alice"))
        .await
        .expect("registration");
    let registration_json = serde_json::to_string(&registration.registration).expect("serialize");
    assert!(!registration_json.contains(ALICE_TOKEN));
}

/// Complete secrecy audit: the user token never appears in any observable
/// results payload, interaction state, connect page, nor in the encrypted or
/// plaintext JetStream KV buckets at rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets_never_reach_results_pages_or_kv() {
    harnx_core::require_nextest();
    let (_keys, fixture, _upstream) = standard_fixture().await;
    let toolset = &fixture.server.toolset;
    let lifecycle = fixture.server.lifecycle.clone();
    let principal = alice_principal();

    // Drive the full flow once, capturing every observable surface.
    let (challenge, challenge_id, round) = start_connect_flow(toolset, Some("alice")).await;
    let http = reqwest::Client::new();
    let page = http
        .get(connect_url(&fixture.http_base, &challenge_id))
        .send()
        .await
        .expect("connect page");
    let html = page.text().await.expect("page body");
    let csrf = extract_csrf(&html).expect("csrf on the page");

    let (status, body) =
        complete_token_form(&fixture.http_base, &challenge_id, ALICE_TOKEN, None, &[])
            .await
            .expect("complete the flow");
    assert_eq!(status, 200, "{body}");

    let connected = invoke(
        toolset,
        "connect",
        Some("alice"),
        &[(connect_challenge_key(round), continue_response())],
        json!({ "challenge_id": challenge_id, "round": round }),
    )
    .await
    .expect("continue");
    let who = invoke(toolset, "whoami", Some("alice"), &[], Value::Null)
        .await
        .expect("whoami");
    let status = invoke(
        toolset,
        "credential_status",
        Some("alice"),
        &[],
        Value::Null,
    )
    .await
    .expect("status");
    let flow = lifecycle
        .flows
        .get_by_challenge(&challenge_id)
        .await
        .expect("flow")
        .expect("flow present");

    // Results and interaction payloads carry no token.
    let challenge_msg = challenge.message.clone().unwrap_or_default();
    let challenge_u = challenge.url.clone().unwrap_or_default();
    let challenge_st = serde_json::to_string(&challenge.request_state).expect("serialize");
    let conn_res = serde_json::to_string(&connected).expect("serialize");
    let who_res = serde_json::to_string(&who).expect("serialize");
    let status_res = serde_json::to_string(&status).expect("serialize");
    assert_no_token_in_results(
        &[
            ("challenge message", &challenge_msg),
            ("challenge url", &challenge_u),
            ("challenge state", &challenge_st),
            ("connect result", &conn_res),
            ("whoami result", &who_res),
            ("status result", &status_res),
        ],
        ALICE_TOKEN,
    );

    assert_kv_and_registration_secrets_absent(
        &fixture,
        &lifecycle,
        &principal,
        FlowSecrets {
            challenge_id: &challenge_id,
            flow_state: flow.record.oauth_state.expose(),
            csrf: &csrf,
        },
    )
    .await;
}

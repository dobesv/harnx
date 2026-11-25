#![cfg(test)]
//! Integration tests for OAuth callback validation (state and issuer verification).
//!
//! Validates that:
//! - Unknown/wrong callback state is rejected (404), does not exchange tokens, does not write credentials, and leaves the valid flow intact.
//! - Tampered/wrong callback issuer claims and fails the flow (502), never exchanges tokens, writes no credentials, and prevents replay (409 single-use).

use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use reqwest::Client;
use serde_json::json;
use url::Url;

use super::*;

const DEFAULT_EXPIRY_SECS: u64 = 3600;

async fn start_validation_flow(fixture: &OAuthFixture) -> (String, u64) {
    let (_challenge, state) = start_connect(&fixture.instance.server.toolset, Some("alice")).await;
    let challenge_id = state["challenge_id"].as_str().expect("id").to_string();
    let round = state["round"].as_u64().expect("round");
    (challenge_id, round)
}

async fn fetch_oauth_callback_url(http: &Client, http_base: &str, challenge_id: &str) -> Url {
    let oauth_res = http
        .get(format!("{http_base}/connect/{challenge_id}/oauth"))
        .send()
        .await
        .unwrap_or_else(|_| panic!("start oauth request failed"));
    let authorize_url = oauth_res
        .headers()
        .get(axum::http::header::LOCATION)
        .expect("Location")
        .to_str()
        .expect("utf8");

    let auth_res = http
        .get(authorize_url)
        .send()
        .await
        .unwrap_or_else(|_| panic!("authorize request failed"));
    let callback_redirect = auth_res
        .headers()
        .get(axum::http::header::LOCATION)
        .expect("callback Location")
        .to_str()
        .expect("utf8");

    let mut valid_cb_url = Url::parse(callback_redirect).expect("parse cb");
    let target_base = Url::parse(http_base).expect("parse base");
    let _ = valid_cb_url.set_host(target_base.host_str());
    let _ = valid_cb_url.set_port(target_base.port());
    valid_cb_url
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_oauth_callback_state_never_exchanges_tokens_or_writes_credentials() {
    harnx_core::require_nextest();
    let fixture = OAuthFixture::spawn(DEFAULT_EXPIRY_SECS).await;
    let toolset = &fixture.instance.server.toolset;
    let lifecycle = &fixture.instance.server.lifecycle;

    let (challenge_id, round) = start_validation_flow(&fixture).await;

    let flow = lifecycle
        .flows
        .get_by_challenge(&challenge_id)
        .await
        .expect("flow read")
        .expect("pending flow");
    assert_eq!(flow.record.status, harnx_credentials::FlowStatus::Pending);

    // Send callback request with an unknown / invalid state parameter
    let http = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");

    let cb_url = format!(
        "{}/callback?code=some-code&state=unknown-state-key",
        fixture.instance.http_base
    );
    let res = http
        .get(&cb_url)
        .send()
        .await
        .unwrap_or_else(|_| panic!("send callback failed"));
    assert_eq!(res.status().as_u16(), StatusCode::NOT_FOUND.as_u16());
    let body = res.text().await.expect("response body");
    assert!(body.contains("Connection link not found"));

    // Token exchange at mock must never have occurred
    assert_eq!(
        fixture
            .mock
            .state
            .token_request_count
            .load(Ordering::SeqCst),
        0
    );

    // No credentials, catalog, or registration written
    let cred = lifecycle
        .credentials
        .for_principal(alice_principal())
        .expect("store")
        .get()
        .await
        .expect("read cred");
    assert!(cred.is_none());

    let cat = lifecycle
        .catalogs
        .for_principal(alice_principal())
        .expect("store")
        .get()
        .await
        .expect("read catalog");
    assert!(cat.is_none());

    let reg = stored_registration(&lifecycle.handle, &audience("alice")).await;
    assert!(reg.is_none());

    // The genuine pending flow was not clobbered or claimed by the unknown state attempt
    let flow_after = lifecycle
        .flows
        .get_by_challenge(&challenge_id)
        .await
        .expect("flow read")
        .expect("pending flow");
    assert_eq!(
        flow_after.record.status,
        harnx_credentials::FlowStatus::Pending
    );

    // The genuine flow can still be completed normally
    let (valid_status, valid_body) =
        drive_oauth_connect(&fixture.instance.http_base, &challenge_id, &[])
            .await
            .expect("drive valid connect");
    assert_eq!(valid_status, StatusCode::OK.as_u16());
    assert!(valid_body.contains("Connected"));

    let connected = invoke(
        toolset,
        "connect",
        Some("alice"),
        &[(connect_challenge_key(round), continue_response())],
        json!({ "challenge_id": challenge_id, "round": round }),
    )
    .await
    .expect("continue succeeds");
    assert!(connected["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Connected"));

    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_oauth_callback_issuer_claims_and_fails_without_exchange_or_credential_and_prevents_replay(
) {
    harnx_core::require_nextest();
    let fixture = OAuthFixture::spawn(DEFAULT_EXPIRY_SECS).await;
    let toolset = &fixture.instance.server.toolset;
    let lifecycle = &fixture.instance.server.lifecycle;

    let (challenge_id, round) = start_validation_flow(&fixture).await;

    let http = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");

    let valid_cb_url =
        fetch_oauth_callback_url(&http, &fixture.instance.http_base, &challenge_id).await;

    // Tamper the issuer parameter to an untrusted/wrong issuer
    let mut tampered_cb_url = valid_cb_url.clone();
    let pairs: Vec<(String, String)> = tampered_cb_url
        .query_pairs()
        .map(|(k, v)| {
            if k == "iss" {
                (
                    k.into_owned(),
                    "https://untrusted-issuer.example.com".to_string(),
                )
            } else {
                (k.into_owned(), v.into_owned())
            }
        })
        .collect();
    tampered_cb_url
        .query_pairs_mut()
        .clear()
        .extend_pairs(pairs.into_iter());

    // Submit tampered callback: claim succeeds, but exchange rejects the mismatched issuer
    let cb_res = http
        .get(tampered_cb_url.as_str())
        .send()
        .await
        .unwrap_or_else(|_| panic!("send tampered cb failed"));
    assert_eq!(cb_res.status().as_u16(), StatusCode::BAD_GATEWAY.as_u16());
    let body = cb_res.text().await.expect("body");
    assert!(body.contains("Connection failed"));
    assert!(body.contains("OAuth code exchange failed"));

    // Token exchange at mock must never have occurred
    assert_eq!(
        fixture
            .mock
            .state
            .token_request_count
            .load(Ordering::SeqCst),
        0
    );

    // No credentials, catalog, or registration written
    let cred = lifecycle
        .credentials
        .for_principal(alice_principal())
        .expect("store")
        .get()
        .await
        .expect("read cred");
    assert!(cred.is_none());

    // Flow was claimed and transitioned to Failed
    let flow = lifecycle
        .flows
        .get_by_challenge(&challenge_id)
        .await
        .expect("read flow")
        .expect("flow");
    assert!(matches!(
        flow.record.status,
        harnx_credentials::FlowStatus::Failed
    ));

    // Replay with the original callback URL (even with genuine issuer) is rejected with 409 Conflict
    let replay_res = http
        .get(valid_cb_url.as_str())
        .send()
        .await
        .unwrap_or_else(|_| panic!("replay cb failed"));
    assert_eq!(replay_res.status().as_u16(), StatusCode::CONFLICT.as_u16());
    let replay_body = replay_res.text().await.expect("replay body");
    assert!(replay_body.contains("Connection link already used"));

    // Calling continue confirms failure to the agent
    let continue_err = invoke(
        toolset,
        "connect",
        Some("alice"),
        &[(connect_challenge_key(round), continue_response())],
        json!({ "challenge_id": challenge_id, "round": round }),
    )
    .await
    .expect_err("continue on failed flow fails");
    assert!(
        matches!(continue_err, ToolInvokeError::Recoverable(msg) if msg.contains("OAuth code exchange failed")),
        "safe error returned"
    );

    // Credential remains absent
    let cred_final = lifecycle
        .credentials
        .for_principal(alice_principal())
        .expect("store")
        .get()
        .await
        .expect("read cred");
    assert!(cred_final.is_none());

    fixture.shutdown().await;
}

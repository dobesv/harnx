#![cfg(test)]
//! Integration tests for OAuth security: callback replay guards, trusted header isolation, and secret leakage prevention.

use std::sync::atomic::Ordering;

use harnx_credentials::flow::challenge_key;
use serde_json::Value;

use super::*;
use crate::native_common::*;

/// Callback replay is rejected with conflict; trusted user header guards callback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn callback_replay_and_trusted_mismatch() {
    harnx_core::require_nextest();
    let mock = MockOAuthServer::spawn(3600).await.expect("spawn mock");
    let extra = ["--trusted-user-header", "x-user-id"];
    let shared = shared_nats(&mock.url, &extra).await;
    let instance = start_instance(&shared).await;
    let toolset = &instance.server.toolset;

    let (challenge_id, _round) = start_connect_flow(toolset, "alice").await;

    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");

    // Request with mismatched user header is refused with 403
    let res = http
        .get(format!(
            "{}/connect/{challenge_id}/oauth",
            instance.http_base
        ))
        .header("x-user-id", "bob")
        .send()
        .await
        .unwrap_or_else(|_| panic!("oauth request with mismatched user header failed"));
    assert_eq!(res.status().as_u16(), 403);

    // Request with correct header gets 307
    let res = http
        .get(format!(
            "{}/connect/{challenge_id}/oauth",
            instance.http_base
        ))
        .header("x-user-id", "alice")
        .send()
        .await
        .unwrap_or_else(|_| panic!("oauth request with correct user header failed"));
    assert_eq!(res.status().as_u16(), 307);
    let authorize_url = res
        .headers()
        .get(axum::http::header::LOCATION)
        .expect("Location header")
        .to_str()
        .expect("Location header utf8");

    let auth_res = http
        .get(authorize_url)
        .send()
        .await
        .unwrap_or_else(|_| panic!("authorize request failed"));
    let callback_loc = auth_res
        .headers()
        .get(axum::http::header::LOCATION)
        .unwrap();
    let callback_url = callback_loc.to_str().expect("callback utf8");

    // Callback with wrong user header is 403
    let cb_wrong = http
        .get(callback_url)
        .header("x-user-id", "bob")
        .send()
        .await
        .unwrap_or_else(|_| panic!("callback with wrong user header failed"));
    assert_eq!(cb_wrong.status().as_u16(), 403);

    // Callback with correct user header succeeds
    let cb_correct = http
        .get(callback_url)
        .header("x-user-id", "alice")
        .send()
        .await
        .unwrap_or_else(|_| panic!("callback with correct user header failed"));
    assert_eq!(cb_correct.status().as_u16(), 200);

    // Replay of the same callback is rejected with 409 Conflict
    let cb_replay = http
        .get(callback_url)
        .header("x-user-id", "alice")
        .send()
        .await
        .unwrap_or_else(|_| panic!("replay callback failed"));
    assert_eq!(cb_replay.status().as_u16(), 409);

    instance.shutdown.cancel();
    mock.shutdown().await;
}

/// Authorization rejection by user / identity provider returns safe error in callback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authorization_rejected_callback_error() {
    harnx_core::require_nextest();
    let fixture = OAuthFixture::spawn(3600).await;
    fixture
        .mock
        .state
        .reject_authorization
        .store(true, Ordering::SeqCst);

    let (challenge_id, _round) = start_connect_flow(fixture.toolset(), "alice").await;

    let (status, body) = drive_oauth_connect(&fixture.instance.http_base, &challenge_id, &[])
        .await
        .expect("connect");
    assert_eq!(status, 502, "{body}");
    assert!(body.contains("OAuth authorization was declined"));

    // No registration was created
    assert!(
        stored_registration(&fixture.lifecycle().handle, &audience("alice"))
            .await
            .is_none()
    );

    fixture.shutdown().await;
}

/// Secret leakage scan:
/// Stored authorization code, access token, refresh token, client secret, and CSRF tokens
/// never appear in tool results, challenge state, HTTP pages, or plaintext KV stores.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets_never_leak_in_oauth_surfaces() {
    harnx_core::require_nextest();
    let mock = MockOAuthServer::spawn(3600).await.expect("spawn mock");
    let shared = shared_nats(&mock.url, &[]).await;
    let instance = start_instance(&shared).await;
    let toolset = &instance.server.toolset;
    let lifecycle = &instance.server.lifecycle;

    let (challenge, challenge_id, round) = start_connect_with_challenge(toolset, "alice").await;

    let (connect_html, csrf) = fetch_connect_page(&instance.http_base, &challenge_id).await;

    let (status, callback_html) = drive_oauth_connect(&instance.http_base, &challenge_id, &[])
        .await
        .expect("connect");
    assert_eq!(status, 200);

    let connected = continue_connect(toolset, "alice", &challenge_id, round).await;

    let who = invoke(toolset, "whoami", Some("alice"), &[], Value::Null)
        .await
        .expect("whoami");
    let status_res = invoke(
        toolset,
        "credential_status",
        Some("alice"),
        &[],
        Value::Null,
    )
    .await
    .expect("status");

    let mut secrets = vec![
        ("auth_code", mock.state.auth_code.clone()),
        ("access_token", mock.state.initial_access_token.clone()),
        ("refresh_token", mock.state.initial_refresh_token.clone()),
        ("client_secret", mock.state.dcr_client_secret.clone()),
    ];
    if let Some(csrf) = csrf {
        secrets.push(("csrf_token", csrf));
    }

    let check_surfaces = [
        (
            "challenge message",
            challenge.message.clone().unwrap_or_default(),
        ),
        ("challenge url", challenge.url.clone().unwrap_or_default()),
        (
            "challenge state",
            serde_json::to_string(&challenge.request_state).unwrap(),
        ),
        ("connect html", connect_html),
        ("callback html", callback_html),
        ("connect result", serde_json::to_string(&connected).unwrap()),
        ("whoami result", serde_json::to_string(&who).unwrap()),
        ("status result", serde_json::to_string(&status_res).unwrap()),
    ];

    // Check NATS KV values
    let buckets = open_encrypted_buckets(&shared.client).await;
    let cred_entry = buckets
        .creds
        .entry(
            &lifecycle
                .credentials
                .for_principal(alice_principal())
                .unwrap()
                .key(),
        )
        .await
        .unwrap()
        .unwrap();
    let flow_entry = buckets
        .flows
        .entry(&challenge_key(&challenge_id))
        .await
        .unwrap()
        .unwrap();
    let catalog_entry = buckets
        .catalogs
        .entry(
            &lifecycle
                .catalogs
                .for_principal(alice_principal())
                .unwrap()
                .key(),
        )
        .await
        .unwrap()
        .unwrap();

    let reg = stored_registration(&lifecycle.handle, &audience("alice"))
        .await
        .unwrap();
    let reg_json = serde_json::to_string(&reg.registration).unwrap();

    let mut all_text_surfaces = check_surfaces.to_vec();
    all_text_surfaces.push(("registration", reg_json));

    let byte_surfaces: [(&str, &[u8]); 3] = [
        ("credentials bucket", &cred_entry.value),
        ("flows bucket", &flow_entry.value),
        ("catalogs bucket", &catalog_entry.value),
    ];

    assert_secrets_absent(&secrets, &all_text_surfaces, &byte_surfaces);

    instance.shutdown.cancel();
    mock.shutdown().await;
}

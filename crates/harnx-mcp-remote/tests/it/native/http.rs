#![cfg(test)]

use clap::Parser;
use harnx_credentials::FlowOptions;
use harnx_mcp_remote::cli::Cli;
use harnx_mcp_remote::native::NativeConfig;
use serde_json::json;

use super::fixture::*;
use crate::native_common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_post_guards_replays_races_and_csrf() {
    harnx_core::require_nextest();
    let (_keys, fixture, _upstream) = standard_fixture().await;
    let toolset = &fixture.server.toolset;
    let lifecycle = fixture.server.lifecycle.clone();

    let (_, state) = start_connect(toolset, Some("alice")).await;
    let challenge_id = state["challenge_id"].as_str().expect("id").to_string();

    // First completion via the raw form; a replay of the same POST is refused
    // with the already-used page and writes nothing (the stored credential's
    // revision does not move).
    let http = reqwest::Client::new();
    let page = http
        .get(connect_url(&fixture.http_base, &challenge_id))
        .send()
        .await
        .expect("connect page");
    assert_eq!(page.status().as_u16(), 200);
    let html = page.text().await.expect("page body");
    let csrf = extract_csrf(&html).expect("csrf on the page");
    let (status, body) = post_token_form(
        &fixture.http_base,
        &challenge_id,
        TokenForm::new(ALICE_TOKEN, &csrf),
        &[],
    )
    .await
    .expect("first completion");
    assert_eq!(status, 200, "{body}");
    let store = lifecycle
        .credentials
        .for_principal(alice_principal())
        .expect("alice store");
    let after_first = store.inspect().await.expect("snapshot");
    let revision = after_first.revision;
    assert!(after_first.record.is_some(), "credential written");
    let (replay_status, body) = post_token_form(
        &fixture.http_base,
        &challenge_id,
        TokenForm::new(ALICE_TOKEN, &csrf),
        &[],
    )
    .await
    .expect("replayed completion");
    assert_eq!(replay_status, 409, "replay refused: {body}");
    let after_replay = store.inspect().await.expect("snapshot");
    assert_eq!(
        after_replay.revision, revision,
        "a replayed POST writes nothing"
    );

    // Two concurrent POSTs on one fresh flow: the claim CAS writes exactly
    // one credential, and the loser gets the conflict page.
    let (_, state) = start_connect(toolset, Some("alice")).await;
    let challenge_id = state["challenge_id"].as_str().expect("id").to_string();
    let first = {
        let (base, id) = (fixture.http_base.clone(), challenge_id.clone());
        tokio::spawn(async move { complete_token_form(&base, &id, ALICE_TOKEN, None, &[]).await })
    };
    let second = {
        let (base, id) = (fixture.http_base.clone(), challenge_id.clone());
        tokio::spawn(async move { complete_token_form(&base, &id, BOB_TOKEN, None, &[]).await })
    };
    let results = [first.await, second.await];
    let mut statuses: Vec<u16> = results
        .into_iter()
        .map(|result| result.expect("join").expect("post").0)
        .collect();
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 409], "exactly one completion wins");
    let after_race = store.inspect().await.expect("snapshot");
    assert!(after_race.record.is_some(), "exactly one credential");
    assert_ne!(after_race.revision, revision, "the winner's write moved on");

    // A CSRF mismatch is refused without writing; the flow stays pending.
    let (_, state) = start_connect(toolset, Some("alice")).await;
    let challenge_id = state["challenge_id"].as_str().expect("id").to_string();
    let (status, body) = post_token_form(
        &fixture.http_base,
        &challenge_id,
        TokenForm::new(ALICE_TOKEN, "wrong-csrf-token"),
        &[],
    )
    .await
    .expect("csrf mismatch post");
    assert_eq!(status, 403, "csrf mismatch refused: {body}");
    let (status, body) =
        complete_token_form(&fixture.http_base, &challenge_id, ALICE_TOKEN, None, &[])
            .await
            .expect("valid post after csrf refusal");
    assert_eq!(status, 200, "the flow is still pending: {body}");

    // A stale-revision credential write is refused: the KV revision CAS is
    // the guard against stale writers.
    let stale = store
        .put_connect(revision, token_input("stale-writer-token"))
        .await;
    assert!(matches!(stale, Err(harnx_credentials::Error::Conflict)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trusted_user_header_guards_the_pages() {
    harnx_core::require_nextest();
    let (_keys, fixture, upstream) = custom_fixture(&["--trusted-user-header", "x-user-id"]).await;
    let toolset = &fixture.server.toolset;

    let (challenge, state) = start_connect(toolset, Some("alice")).await;
    let challenge_id = state["challenge_id"].as_str().expect("id").to_string();
    assert_eq!(challenge.kind, "url");

    let http = reqwest::Client::new();
    // A missing or mismatched header gets 403 with no writes on both the page
    // and the completing POST.
    for header in [None, Some("mallory")] {
        let mut page = http.get(connect_url(&fixture.http_base, &challenge_id));
        if let Some(value) = header {
            page = page.header("x-user-id", value);
        }
        let response = page.send().await.expect("guarded page");
        assert_eq!(response.status().as_u16(), 403, "page refused: {header:?}");
        let csrf = "any-csrf"; // never reached: the gate refuses first
        let (status, body) = post_token_form(
            &fixture.http_base,
            &challenge_id,
            TokenForm::new(ALICE_TOKEN, csrf),
            &[("x-user-id", header.unwrap_or("mallory"))],
        )
        .await
        .expect("guarded post");
        assert_eq!(status, 403, "post refused: {header:?} {body}");
    }

    // Nothing was written: the correct header still completes the same flow.
    let (status, body) = complete_token_form(
        &fixture.http_base,
        &challenge_id,
        ALICE_TOKEN,
        None,
        &[("x-user-id", "alice")],
    )
    .await
    .expect("correct header");
    assert_eq!(status, 200, "the flow is still pending: {body}");
    let round = state["round"].as_u64().expect("round");
    let result = invoke(
        toolset,
        "connect",
        Some("alice"),
        &[(connect_challenge_key(round), continue_response())],
        json!({ "challenge_id": challenge_id, "round": round }),
    )
    .await
    .expect("continue with the header-guarded flow");
    assert!(
        result["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("Connected; 2 jira tools"),
        "{result}"
    );
    let _ = upstream.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_base_url_routes_under_its_path() {
    harnx_core::require_nextest();
    let (_keys, fixture, _upstream) =
        custom_fixture(&["--public-base-url", "https://example.test/tools/jira"]).await;

    let (challenge, challenge_id, _) =
        start_connect_flow(&fixture.server.toolset, Some("alice")).await;
    let url = challenge.url.expect("link");
    assert!(
        url.starts_with("https://example.test/tools/jira/connect/"),
        "link uses the public base: {url}"
    );

    // The pages answer under the configured path on the local listener.
    let http = reqwest::Client::new();
    let page = http
        .get(format!(
            "{}/tools/jira/connect/{challenge_id}",
            fixture.http_base
        ))
        .send()
        .await
        .expect("page under subpath");
    assert_eq!(page.status().as_u16(), 200);
    let root_miss = http
        .get(connect_url(&fixture.http_base, &challenge_id))
        .send()
        .await
        .expect("page at the root");
    assert_eq!(
        root_miss.status().as_u16(),
        404,
        "routes only under the base path"
    );
}

#[test]

fn native_startup_validation_rejects_unsafe_configurations() {
    // --bearer-token: no process-wide credential may act as a fallback (R7).
    let cli = Cli::parse_from([
        "harnx-mcp-remote",
        "--url",
        "https://example.test/mcp",
        "--bearer-token",
        "secret",
        "--credentials-key-file",
        "/nonexistent/keys.json",
        "--name",
        "jira",
    ]);
    let error = NativeConfig::from_cli(&cli).expect_err("bearer token rejected");
    assert!(error.to_string().contains("bearer-token"), "{error:#}");

    // A non-loopback --http-bind requires --public-base-url.
    let keys = tempfile::TempDir::new().expect("temp dir");
    let key_file = write_key_file(&keys);
    let cli = native_cli(
        "https://example.test/mcp",
        &key_file,
        &["--http-bind", "0.0.0.0:9999"],
    );
    let error = NativeConfig::from_cli(&cli).expect_err("public bind rejected");
    assert!(error.to_string().contains("public-base-url"), "{error:#}");

    // --single-user rejects a trusted header and a non-loopback bind.
    let cli = native_cli(
        "https://example.test/mcp",
        &key_file,
        &["--single-user", "--trusted-user-header", "x-user-id"],
    );
    let error = NativeConfig::from_cli(&cli).expect_err("single-user + trusted header rejected");
    assert!(error.to_string().contains("--single-user"), "{error:#}");
    let cli = native_cli(
        "https://example.test/mcp",
        &key_file,
        &["--single-user", "--http-bind", "0.0.0.0:9999"],
    );
    let error = NativeConfig::from_cli(&cli).expect_err("single-user public bind rejected");
    assert!(error.to_string().contains("--single-user"), "{error:#}");

    // A non-HTTPS upstream without --insecure refuses per-user credentials.
    let cli = native_cli("http://example.test/mcp", &key_file, &[]);
    let error = NativeConfig::from_cli(&cli).expect_err("plaintext upstream rejected");
    assert!(error.to_string().contains("HTTPS"), "{error:#}");

    // --auth-methods must name at least one real method.
    let cli = native_cli(
        "https://example.test/mcp",
        &key_file,
        &["--auth-methods", "nope"],
    );
    let error = NativeConfig::from_cli(&cli).expect_err("unknown method rejected");
    assert!(error.to_string().contains("--auth-methods"), "{error:#}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_flow_link_gets_an_error_page() {
    harnx_core::require_nextest();
    let (_keys, fixture, _upstream) = standard_fixture().await;
    let lifecycle = fixture.server.lifecycle.clone();

    // A flow whose link already expired (the T5 expiry test hook on the flow
    // store; production flows always get the 10-minute TTL).
    let flow = lifecycle
        .flows
        .create_with_expiry(
            alice_principal(),
            FlowOptions {
                oauth_context: None,
                redirect_uri: format!("{}/callback", fixture.http_base),
                scopes: vec![],
                oauth_client: None,
                authorization_state: None,
            },
            chrono::Utc::now() - chrono::TimeDelta::minutes(1),
        )
        .await
        .expect("expired flow");
    let challenge_id = flow.record.challenge_id.expose().to_string();

    let http = reqwest::Client::new();
    let page = http
        .get(connect_url(&fixture.http_base, &challenge_id))
        .send()
        .await
        .expect("expired page");
    assert_eq!(page.status().as_u16(), 410, "expired link: gone page");
    let body = page.text().await.expect("page body");
    assert!(body.contains("expired"), "{body}");

    let (status, body) = post_token_form(
        &fixture.http_base,
        &challenge_id,
        TokenForm::new(ALICE_TOKEN, "csrf"),
        &[],
    )
    .await
    .expect("post on an expired link");
    assert_eq!(status, 410, "the POST is also refused: {body}");

    let store = lifecycle
        .credentials
        .for_principal(alice_principal())
        .expect("store");
    assert!(
        store.inspect().await.expect("snapshot").record.is_none(),
        "an expired link writes nothing"
    );
}

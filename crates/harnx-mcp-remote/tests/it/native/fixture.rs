#![cfg(test)]

use std::time::Duration;

use harnx_credentials::{
    CredentialMaterial, CredentialRecord, NewCredential, Principal, Secret, TokenMaterial,
    Versioned,
};
use harnx_mcp_remote::native::registrations::Lifecycle;
use harnx_mcp_remote::native::toolset::RemoteCredentialsToolset;
use harnx_mcp_remote::native::{NativeConfig, NativeServer};
use harnx_toolset::{ToolInputRequired, ToolInvokeError};
use harnx_toolset_server::{HandleAudience, StoredRegistration};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::native_common::*;

pub fn audience(user: &str) -> HandleAudience {
    HandleAudience::User(user.to_string())
}

pub fn alice_principal() -> Principal {
    Principal::User("alice".into())
}

pub fn connect_url(http_base: &str, challenge_id: &str) -> String {
    format!("{http_base}/connect/{challenge_id}")
}

pub async fn custom_fixture(extra: &[&str]) -> (tempfile::TempDir, NativeFixture, UpstreamHandle) {
    let upstream = spawn_per_user_upstream("authorization", false)
        .await
        .expect("start mock upstream");
    let keys = tempfile::TempDir::new().expect("temp dir");
    let key_file = write_key_file(&keys);
    let cli = native_cli(&upstream.url, &key_file, extra);
    let fixture = start_native(config_from_cli(&cli))
        .await
        .expect("start native");
    (keys, fixture, upstream)
}

pub async fn standard_fixture() -> (tempfile::TempDir, NativeFixture, UpstreamHandle) {
    custom_fixture(&[]).await
}

/// A fixture against an already-spawned upstream, for upstream variants the
/// standard fixture doesn't build (F35 leak tests).
pub async fn fixture_on(upstream: &UpstreamHandle) -> (tempfile::TempDir, NativeFixture) {
    let keys = tempfile::TempDir::new().expect("temp dir");
    let key_file = write_key_file(&keys);
    let cli = native_cli(&upstream.url, &key_file, &[]);
    let fixture = start_native(config_from_cli(&cli))
        .await
        .expect("start native");
    (keys, fixture)
}

/// `fixture_on` plus a connected alice: the preamble every per-user native
/// test against a custom upstream starts with.
pub async fn alice_fixture(upstream: &UpstreamHandle) -> (tempfile::TempDir, NativeFixture) {
    let (keys, fixture) = fixture_on(upstream).await;
    connect_user(
        &fixture.server.toolset,
        &fixture.http_base,
        Some("alice"),
        ALICE_TOKEN,
    )
    .await
    .expect("connect alice");
    (keys, fixture)
}

pub struct Instance {
    pub server: NativeServer,
    pub http_base: String,
    pub shutdown: CancellationToken,
}

pub struct SharedNats {
    pub _keys: tempfile::TempDir,
    pub config: NativeConfig,
    pub client: async_nats::Client,
    #[allow(dead_code)]
    pub nats: harnx_test_bins::NatsServerHandle,
}

pub async fn shared_nats(upstream_url: &str, extra: &[&str]) -> SharedNats {
    let nats = harnx_test_bins::spawn_nats_server()
        .await
        .expect("spawn NATS")
        .expect("NATS tests require nats-server; set NATS_SERVER_BIN (no silent skips)");
    let client = async_nats::connect(nats.url()).await.expect("connect NATS");
    let keys = tempfile::TempDir::new().expect("temp dir");
    let key_file = write_key_file(&keys);
    let cli = native_cli(upstream_url, &key_file, extra);
    SharedNats {
        _keys: keys,
        config: config_from_cli(&cli),
        client,
        nats,
    }
}

pub async fn multi_instance_fixture(extra: &[&str]) -> (SharedNats, UpstreamHandle) {
    let upstream = spawn_per_user_upstream("authorization", false)
        .await
        .expect("upstream");
    let shared = shared_nats(&upstream.url, extra).await;
    (shared, upstream)
}

pub async fn start_instance(shared: &SharedNats) -> Instance {
    let shutdown = CancellationToken::new();
    let server = start_native_on(shared.config.clone(), &shared.client, shutdown.clone())
        .await
        .expect("start instance");
    let http_base = format!("http://{}", server.local_addr);
    Instance {
        server,
        http_base,
        shutdown,
    }
}

pub async fn start_connect(
    toolset: &RemoteCredentialsToolset,
    user: Option<&str>,
) -> (ToolInputRequired, Value) {
    let error = invoke(toolset, "connect", user, &[], Value::Null)
        .await
        .expect_err("connect starts a url challenge");
    url_challenge(error)
}

pub async fn start_connect_flow(
    toolset: &RemoteCredentialsToolset,
    user: Option<&str>,
) -> (ToolInputRequired, String, u64) {
    let (challenge, state) = start_connect(toolset, user).await;
    let challenge_id = state["challenge_id"]
        .as_str()
        .expect("challenge id")
        .to_string();
    let round = state["round"].as_u64().expect("round");
    (challenge, challenge_id, round)
}

pub async fn connect_user(
    toolset: &RemoteCredentialsToolset,
    http_base: &str,
    user: Option<&str>,
    token: &str,
) -> Result<Value, ToolInvokeError> {
    let (challenge, challenge_id, round) = start_connect_flow(toolset, user).await;
    assert_eq!(challenge.kind, "url", "the connect challenge is a url link");
    let url = challenge.url.as_deref().expect("challenge url");
    assert!(
        url.starts_with(http_base),
        "link uses the server base: {url}"
    );
    let (status, body) = complete_token_form(http_base, &challenge_id, token, None, &[])
        .await
        .expect("complete token form");
    assert_eq!(status, 200, "token form completion: {body}");
    let responses = vec![(connect_challenge_key(round), continue_response())];
    invoke(
        toolset,
        "connect",
        user,
        &responses,
        json!({ "challenge_id": challenge_id, "round": round }),
    )
    .await
}

pub async fn credential_of(
    lifecycle: &Lifecycle,
    principal: &Principal,
) -> Versioned<CredentialRecord> {
    let store = lifecycle
        .credentials
        .for_principal(principal.clone())
        .expect("credential store");
    let snapshot = store.inspect().await.expect("credential snapshot");
    Versioned {
        revision: snapshot.revision,
        record: snapshot.record.expect("credential"),
    }
}

pub fn token_input(token: &str) -> NewCredential {
    NewCredential {
        material: CredentialMaterial::Token(TokenMaterial {
            token: Secret::new(token.to_string()),
            header: "authorization".into(),
            scheme: Some("Bearer".into()),
        }),
        account_label: None,
        expires_at: None,
    }
}

pub async fn wait_for_catalog_tool(
    lifecycle: &Lifecycle,
    principal: &Principal,
    want: &str,
) -> Versioned<harnx_credentials::Catalog> {
    let catalogs = lifecycle
        .catalogs
        .for_principal(principal.clone())
        .expect("catalog store");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(catalog) = catalogs.get().await.expect("catalog read") {
            if catalog.record.tools.iter().any(|tool| &*tool.name == want) {
                return catalog;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for the catalog to contain {want}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub async fn wait_for_registration_tool(
    lifecycle: &Lifecycle,
    user_audience: &HandleAudience,
    want: &str,
) -> StoredRegistration {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(stored) = stored_registration(&lifecycle.handle, user_audience).await {
            if stored
                .registration
                .tools
                .iter()
                .any(|tool| tool.name == want)
            {
                return stored;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for the registration to contain {want}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

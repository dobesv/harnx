//! Real NATS dispatch and invocation journaling, with production process logs.
#![cfg(test)]
use super::*;
use futures::StreamExt;
use harnx_core::instance::ServerScope;
use harnx_credentials::{CredentialStore, Keyring, PendingFlowStore};
use harnx_toolset::{ToolReply, ToolRequest};
use std::process::Stdio;
use tokio::io::AsyncReadExt;

mod process;
use process::{BusCapture, NativeProcess};

fn assert_secret_free(bytes: &[u8], secrets: &[String], surface: &str) {
    for secret in secrets {
        assert!(!secret.is_empty());
        assert!(
            !bytes_contain(bytes, secret.as_bytes()),
            "OAuth secret leaked via {surface}"
        );
    }
}

struct WireClient<'a> {
    client: &'a async_nats::Client,
    scope: &'a ServerScope,
    identity: &'a str,
    surfaces: Vec<Vec<u8>>,
}

impl WireClient<'_> {
    async fn call(
        &mut self,
        tool: &str,
        state: Option<Value>,
        continue_key: Option<String>,
    ) -> ToolReply {
        let id = format!("oauth-journal-{}", self.surfaces.len());
        let responses = continue_key.map(|key| {
            [(key, continue_response())]
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>()
        });
        let request: ToolRequest = serde_json::from_value(json!({
            "operation_id": id,
            "call_id": id,
            "tool": tool,
            "args": {},
            "user_id": "alice",
            "input_responses": responses,
            "request_state": state,
        }))
        .expect("request shape");
        let bytes = serde_json::to_vec(&request).expect("request encoding");
        self.surfaces.push(bytes.clone());
        let mut headers = async_nats::HeaderMap::new();
        headers.insert(harnx_toolset::HDR_IDEMPOTENCY_KEY, id.as_str());
        headers.insert(harnx_toolset::HDR_CALL_ID, id.as_str());
        let reply = tokio::time::timeout(
            Duration::from_secs(20),
            self.client.request_with_headers(
                self.scope.tool_subject(self.identity, tool),
                headers,
                bytes.into(),
            ),
        )
        .await
        .expect("native tool request deadline")
        .expect("native tool request failed");
        self.surfaces.push(reply.payload.to_vec());
        serde_json::from_slice(&reply.payload).expect("tool reply shape")
    }
}

fn collect_flow_secrets(flow: &harnx_credentials::PendingFlow, secrets: &mut Vec<String>) {
    secrets.push(flow.oauth_state.expose().clone());
    secrets.push(flow.csrf_token.expose().clone());
    secrets.push(
        flow.pkce_verifier
            .as_ref()
            .expect("PKCE verifier")
            .expose()
            .clone(),
    );
    let client = flow.oauth_client.as_ref().expect("client registration");
    if let Some(secret) = &client.client_secret {
        secrets.push(secret.expose().clone());
        secrets.push(STANDARD.encode(format!("{}:{}", client.client_id, secret.expose())));
    }
}

fn reject_current_access_token(mock: &MockOAuthServer, record: &CredentialRecord) {
    let CredentialMaterial::Oauth2(material) = &record.material else {
        panic!("expected OAuth credential");
    };
    mock.state
        .rejected_tokens
        .lock()
        .unwrap()
        .insert(material.access_token.expose().clone());
}

async fn scan_invocation_journal(js: &async_nats::jetstream::Context, secrets: &[String]) {
    // Scan real durable rows, not a serialized stand-in for journal writes.
    let journal = js
        .get_key_value(harnx_toolset_server::invocation_journal::BUCKET)
        .await
        .expect("invocation journal");
    let mut keys = journal.keys().await.expect("journal keys");
    let mut rows = 0;
    while let Some(key) = keys.next().await {
        let entry = journal
            .entry(key.expect("journal key"))
            .await
            .expect("journal read")
            .expect("journal row");
        assert_secret_free(&entry.value, secrets, "invocation journal");
        rows += 1;
    }
    assert!(rows >= 4, "real invocation journal wasn't exercised");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oauth_secrets_never_reach_process_logs_nats_or_invocation_journal() {
    harnx_core::require_nextest();
    let mock = MockOAuthServer::spawn(3600)
        .await
        .expect("mock OAuth server");
    let shared = shared_nats(&mock.url, &[]).await;
    let scope = ServerScope::from_string(format!("oauth-leak-{}", std::process::id()));
    let identity = harnx_toolset::server_identity_token(None, "", "jira");
    let js = async_nats::jetstream::new(shared.client.clone());
    let keys = Arc::new(Keyring::load(&shared.config.credentials_key_file).expect("test keyring"));
    let flows = PendingFlowStore::open(&js, 1, keys.clone(), identity.clone())
        .await
        .expect("flow store");
    let credentials = CredentialStore::open(&js, 1, keys, identity.clone(), alice_principal())
        .await
        .expect("credential store");
    // Session writers receive only bus payloads. HTTP-only secrets must never
    // reach the bus, including headers or the invocation journal.
    let capture = BusCapture::start(&shared.client).await;
    let mut process = NativeProcess::start(&shared, &mock.url, &scope).await;
    process.wait_registered(&js, &scope, &identity).await;
    let mut wire = WireClient {
        client: &shared.client,
        scope: &scope,
        identity: &identity,
        surfaces: Vec::new(),
    };
    let challenge_reply = wire.call("connect", None, None).await;
    assert!(
        challenge_reply.input_required.is_some(),
        "connect did not return a URL challenge"
    );
    let challenge = challenge_reply.input_required.expect("URL challenge");
    let request_state = challenge.request_state.clone();
    let challenge_id = request_state["challenge_id"]
        .as_str()
        .expect("challenge id");
    let flow = flows
        .get_by_challenge(challenge_id)
        .await
        .expect("flow read")
        .expect("pending flow");
    let mut secrets = vec![mock.state.auth_code.clone()];
    collect_flow_secrets(&flow.record, &mut secrets);
    let connect_url =
        Url::parse(challenge.url.as_deref().expect("connect URL")).expect("connect URL shape");
    let base = connect_url[..url::Position::BeforePath].to_string();
    let (status, html) = drive_oauth_connect(&base, challenge_id, &[])
        .await
        .unwrap_or_else(|_| panic!("OAuth browser request failed"));
    assert_eq!(status, 200);
    wire.surfaces.push(html.into_bytes());
    let initial = credentials
        .get()
        .await
        .expect("credential read")
        .expect("connected credential");
    collect_credential_secrets(&initial.record, &mut secrets);
    let connected = wire
        .call("connect", Some(request_state), Some(challenge.key))
        .await;
    assert!(connected.result.is_ok());
    reject_current_access_token(&mock, &initial.record);
    let result = wire.call("whoami", None, None).await;
    assert!(result.result.is_ok(), "authenticated tool call failed");
    assert_eq!(mock.state.refresh_count.load(Ordering::SeqCst), 1);
    let refreshed = credentials
        .get()
        .await
        .expect("refreshed read")
        .expect("refreshed credential");
    collect_credential_secrets(&refreshed.record, &mut secrets);
    assert_eq!(refreshed.record.connection_id, initial.record.connection_id);
    mock.state.invalid_grant.store(true, Ordering::SeqCst);
    reject_current_access_token(&mock, &refreshed.record);
    let expired = wire.call("whoami", None, None).await;
    assert!(
        matches!(expired.result, Err(harnx_toolset::ToolErrorPayload::Recoverable(message))
        if message.contains("jira_connect")),
        "invalid_grant must return a safe reconnect hint"
    );
    scan_invocation_journal(&js, &secrets).await;
    for bytes in &wire.surfaces {
        assert_secret_free(bytes, &secrets, "tool request/reply and browser response");
    }
    capture.stop_and_scan(&shared.client, &secrets).await;
    process.stop_and_scan(&secrets).await;
    mock.shutdown().await;
}

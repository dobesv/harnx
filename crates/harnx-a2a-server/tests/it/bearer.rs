//! Verified bearer identity over HTTP: 401 / -32000 fail-closed, persisted DN
//! ownership, and no JWKS fetch without verification flags.

use crate::{
    support::{Harness, Script},
    unary::{message, Http},
};
use anyhow::Result;
use axum::{routing::get, Router};
use harnx_a2a_server::{bearer_verify::BearerVerifier, identity::Identity};
use harnx_core::{access_rules::AccessRules, session_identity::session_key};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

const ISSUER: &str = "forge/invocation-token";
const AUDIENCE: &str = "ari:cloud:ecosystem::app/test-app";
const DN: &str = "CN=557058:account-id,OU=d0d52620-3203-4cfa-8db5-f2587155f0dd,O=atlassian.com";
const PRIMARY_PEM: &str = include_str!("../fixtures/bearer/primary.pem");
const SECONDARY_PEM: &str = include_str!("../fixtures/bearer/secondary.pem");
const JWKS_PRIMARY: &str = include_str!("../fixtures/bearer/jwks-primary.json");

/// Local JWKS server that counts fetches.
struct Jwks {
    url: String,
    fetches: Arc<AtomicU64>,
}

impl Jwks {
    async fn start() -> Result<Self> {
        let fetches = Arc::new(AtomicU64::new(0));
        let counter = fetches.clone();
        let app = Router::new().route(
            "/.well-known/jwks.json",
            get(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async { JWKS_PRIMARY }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/.well-known/jwks.json", listener.local_addr()?);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Ok(Self { url, fetches })
    }

    fn fetches(&self) -> u64 {
        self.fetches.load(Ordering::SeqCst)
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn forge_claims() -> Value {
    json!({
        "iss": ISSUER,
        "aud": AUDIENCE,
        "iat": now(),
        "exp": now() + 3600,
        "principal": "557058:account-id",
        "context": {"cloudId": "d0d52620-3203-4cfa-8db5-f2587155f0dd"},
    })
}

fn sign(claims: &Value, kid: &str, pem: &str) -> String {
    encode(
        &Header {
            alg: Algorithm::RS256,
            kid: Some(kid.into()),
            ..Default::default()
        },
        claims,
        &EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap(),
    )
    .unwrap()
}

fn valid_token() -> String {
    sign(&forge_claims(), "primary-2026-01", PRIMARY_PEM)
}

/// Signed with a key that isn't in the served JWKS.
fn forged_token() -> String {
    sign(&forge_claims(), "primary-2026-01", SECONDARY_PEM)
}

fn rules() -> Arc<AccessRules> {
    Arc::new(
        AccessRules::from_yaml(
            r#"
rules:
  - agents: [runner@runner]
    users: ["CN=*,OU=d0d52620-3203-4cfa-8db5-f2587155f0dd,O=atlassian.com"]
"#,
        )
        .unwrap(),
    )
}

/// Start the full HTTP stack with a forge-fit identity policy and a verifier
/// pointing at the local JWKS.
async fn start_verified(rules: Option<Arc<AccessRules>>) -> Result<(Http, Jwks)> {
    let jwks = Jwks::start().await?;
    let harness = Harness::start_with_access_rules(Script::Text, rules.clone()).await?;
    let identity = Identity::new(&["forge-fit".into()])?.with_bearer_verification(Arc::new(
        BearerVerifier::new(harnx_a2a_server::bearer_verify::BearerVerificationConfig {
            jwks_url: jwks.url.clone(),
            issuer: ISSUER.to_owned(),
            audience: AUDIENCE.to_owned(),
        })?,
    ));
    let http = Http::with_identity(harness, rules, identity).await?;
    Ok((http, jwks))
}

async fn bearer_rpc(
    http: &Http,
    token: &str,
    method: &str,
    params: Value,
) -> Result<(reqwest::StatusCode, Value)> {
    let response = http
        .client
        .post(&http.url)
        .header("a2a-version", "1.0")
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"jsonrpc":"2.0","id":"1","method":method,"params":params}))
        .send()
        .await?;
    let status = response.status();
    let body = response.json().await?;
    Ok((status, body))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bearer_verified_rpc_admits_valid_and_rejects_invalid_tokens() -> Result<()> {
    let (http, jwks) = start_verified(None).await?;
    // Invalid tokens fail closed as 401 / -32000 with a reason category only.
    let (status, body) = bearer_rpc(
        &http,
        &forged_token(),
        "SendMessage",
        json!({"message":message("first", None)}),
    )
    .await?;
    assert_eq!(status, 401);
    assert_eq!(body["error"]["code"], -32000);
    assert_eq!(body["error"]["message"], "bad bearer token signature");
    assert!(body.get("result").is_none());
    assert!(!format!("{body}").contains("Bearer"));

    // A missing Authorization header fails closed the same way.
    let response = http
        .client
        .post(&http.url)
        .header("a2a-version", "1.0")
        .json(&json!({"jsonrpc":"2.0","id":"1","method":"SendMessage","params":{"message":message("none", None)}}))
        .send()
        .await?;
    assert_eq!(response.status(), 401);

    // A valid token resolves the DN and completes a task.
    let token = valid_token();
    http.h.llm.release.notify_one();
    let (status, body) = bearer_rpc(
        &http,
        &token,
        "SendMessage",
        json!({"message":message("first", None)}),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    assert!(body["error"].is_null(), "{body}");
    let task = body["result"]["task"].clone();
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");

    // Persisted ownership is the DN; no token or claim text is stored.
    let key = session_key(Some("runner"), task["contextId"].as_str().unwrap());
    let binding = http.h.store.get_binding(&key).await?.unwrap();
    assert_eq!(binding.owner.as_deref(), Some(DN));
    let record = http.h.metadata.get(&key).await?.unwrap();
    let properties = harnx_runtime::nats_session_metadata::session_properties(&record.metadata)?;
    assert_eq!(properties.text("user_id"), Some(DN));
    let metadata = serde_json::to_string(&record.metadata)?;
    assert!(!metadata.contains(&token), "token persisted: {metadata}");
    assert!(
        !metadata.contains("Bearer "),
        "authorization persisted: {metadata}"
    );
    assert!(!format!("{binding:?}").contains(&token));

    // The shared verifier caches the key set across both resolution paths.
    assert_eq!(jwks.fetches(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bearer_verified_cards_reject_invalid_and_admit_valid_tokens() -> Result<()> {
    let (http, _jwks) = start_verified(Some(rules())).await?;
    let root = http.url.strip_suffix("/runner").unwrap();
    let card_path = format!("{root}/runner/.well-known/agent-card.json");

    let invalid = http
        .client
        .get(&card_path)
        .header("authorization", format!("Bearer {}", forged_token()))
        .send()
        .await?;
    assert_eq!(invalid.status(), 401);

    let valid = http
        .client
        .get(&card_path)
        .header("authorization", format!("Bearer {}", valid_token()))
        .send()
        .await?;
    assert_eq!(valid.status(), 200);
    let card: Value = valid.json().await?;
    assert!(card["name"].is_string());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bearer_flags_absent_decode_only_without_any_jwks_fetch() -> Result<()> {
    let jwks = Jwks::start().await?;
    let harness = Harness::start(Script::Text).await?;
    let http = Http::with_identity(harness, None, Identity::new(&["forge-fit".into()])?).await?;
    // No verification flags: the token is decoded, never verified, never fetched.
    http.h.llm.release.notify_one();
    let token = valid_token();
    let (status, body) = bearer_rpc(
        &http,
        &token,
        "SendMessage",
        json!({"message":message("first", None)}),
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    assert!(body["error"].is_null(), "{body}");
    let task = body["result"]["task"].clone();
    let key = session_key(Some("runner"), task["contextId"].as_str().unwrap());
    assert_eq!(
        http.h
            .store
            .get_binding(&key)
            .await?
            .unwrap()
            .owner
            .as_deref(),
        Some(DN)
    );
    assert_eq!(jwks.fetches(), 0);
    Ok(())
}

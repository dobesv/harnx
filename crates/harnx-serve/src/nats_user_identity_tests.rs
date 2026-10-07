//! Request identity is recorded once, when canonical metadata is created.

use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use harnx_runtime::config::{ConfigLock, NatsRouting};
use harnx_runtime::nats_session_metadata::{session_properties, SessionMetadataStore};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};

use crate::{test_support::TestConfigSandbox, Server};

const CLUSTER: &str = "identity-test";

struct Fixture {
    _nats: harnx_test_bins::NatsServerHandle,
    _sandbox: TestConfigSandbox,
    store: SessionMetadataStore,
    server: Arc<Server>,
    handle: crate::ServerHandle,
    base: String,
    client: Client,
}

struct FixtureIdentity<'a> {
    sources: &'a [&'a str],
    cluster_user: Option<&'a str>,
    global_user: Option<&'a str>,
}

struct Headers<'a>(&'a [(&'a str, &'a str)]);

async fn fixture(identity: FixtureIdentity<'_>) -> Result<Option<Fixture>> {
    let FixtureIdentity {
        sources,
        cluster_user,
        global_user,
    } = identity;
    harnx_core::require_nextest();
    let Some(nats) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(None);
    };
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let mut cluster_config = format!("url: {}\n", nats.url());
    if let Some(user) = cluster_user {
        cluster_config.push_str(&format!("user_id: {user}\n"));
    }
    sandbox.write_nats_server(CLUSTER, &cluster_config);
    let mut config = sandbox.config();
    config.nats_routing = NatsRouting::Cluster(CLUSTER.to_string());
    config.serve_user_id_sources = sources.iter().map(|s| s.to_string()).collect();
    config.user_id = global_user.map(str::to_string);
    let jetstream = config.nats_jetstream(CLUSTER).await?;
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let server = Arc::new(Server::new(
        &Arc::new(ConfigLock::new(config)),
        PathBuf::from("web-assets"),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}/v1/agents/plain/sessions", listener.local_addr()?);
    let handle = server
        .clone()
        .run(listener, crate::DEFAULT_DRAIN_TIMEOUT)
        .await?;
    Ok(Some(Fixture {
        _nats: nats,
        _sandbox: sandbox,
        store,
        server,
        handle,
        base,
        client: Client::new(),
    }))
}

impl Fixture {
    async fn user_id(&self, session: &str) -> Result<Option<String>> {
        let record = self
            .store
            .get_for_agent(session, "plain")
            .await?
            .expect("metadata exists");
        Ok(session_properties(&record.metadata)?
            .text("user_id")
            .map(str::to_string))
    }

    async fn create(&self, headers: Headers<'_>) -> Result<String> {
        let Headers(headers) = headers;
        let mut request = self
            .client
            .post(&self.base)
            .header("accept", "application/json");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: Value = response.json().await?;
        Ok(body["session_id"]
            .as_str()
            .expect("reserved session id")
            .to_string())
    }

    async fn prompt(&self, session: &str, headers: Headers<'_>) -> Result<()> {
        let Headers(headers) = headers;
        let mut request = self.client.post(format!("{}/{session}", self.base)).json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/prompt", "params": {"text": "hello", "user_id": "untrusted-json"}
        }));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await?;
        assert!(
            matches!(
                body["result"]["status"].as_str(),
                Some("accepted" | "enqueued")
            ),
            "{body}"
        );
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_create_stores_header_cookie_and_default_identity_in_summaries() -> Result<()> {
    let Some(f) = fixture(FixtureIdentity {
        sources: &["header:x-user", "cookie:owner"],
        cluster_user: Some("cluster-owner"),
        global_user: Some("global-owner"),
    })
    .await?
    else {
        return Ok(());
    };
    let header = f
        .create(Headers(&[
            ("x-user", "header-owner, ignored"),
            ("cookie", "owner=cookie-owner"),
        ]))
        .await?;
    let cookie = f
        .create(Headers(&[(
            "cookie",
            "unrelated=value; owner=cookie-owner",
        )]))
        .await?;
    let missing = f.create(Headers(&[])).await?;
    assert_eq!(f.user_id(&header).await?.as_deref(), Some("header-owner"));
    assert_eq!(f.user_id(&cookie).await?.as_deref(), Some("cookie-owner"));
    assert_eq!(f.user_id(&missing).await?.as_deref(), Some("cluster-owner"));
    let summaries: Value = f
        .client
        .get(&f.base)
        .header("accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    for (session, user) in [
        (&header, "header-owner"),
        (&cookie, "cookie-owner"),
        (&missing, "cluster-owner"),
    ] {
        let summary = summaries
            .as_array()
            .expect("summary list")
            .iter()
            .find(|v| v["session_id"] == *session)
            .expect("session listed");
        assert_eq!(summary["user_id"], user);
    }
    // Explicit creation also remains immutable on later prompts from another caller.
    f.prompt(&header, Headers(&[("x-user", "later-owner")]))
        .await?;
    assert_eq!(f.user_id(&header).await?.as_deref(), Some("header-owner"));
    f.handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_request_identity_uses_global_default() -> Result<()> {
    let Some(f) = fixture(FixtureIdentity {
        sources: &["x-user"],
        cluster_user: None,
        global_user: Some("global-owner"),
    })
    .await?
    else {
        return Ok(());
    };
    let session = f.create(Headers(&[])).await?;
    assert_eq!(f.user_id(&session).await?.as_deref(), Some("global-owner"));
    f.prompt("implicit-default", Headers(&[])).await?;
    assert_eq!(
        f.user_id("implicit-default").await?.as_deref(),
        Some("global-owner")
    );
    f.handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn absent_sources_ignore_headers_and_summary_identity_is_null() -> Result<()> {
    let Some(f) = fixture(FixtureIdentity {
        sources: &[],
        cluster_user: None,
        global_user: None,
    })
    .await?
    else {
        return Ok(());
    };
    let session = f.create(Headers(&[("x-user", "")])).await?;
    assert_eq!(f.user_id(&session).await?, None);
    let summaries: Value = f
        .client
        .get(&f.base)
        .header("accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let summary = summaries
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["session_id"] == session)
        .unwrap();
    assert!(summary.as_object().unwrap().contains_key("user_id"));
    assert_eq!(summary["user_id"], Value::Null);
    f.handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_identity_rejects_api_before_creating_sessions_or_actors() -> Result<()> {
    let Some(f) = fixture(FixtureIdentity {
        sources: &["x-user", "cookie:owner"],
        cluster_user: Some("cluster-owner"),
        global_user: None,
    })
    .await?
    else {
        return Ok(());
    };
    let rpc =
        json!({"jsonrpc":"2.0", "id":1, "method":"session/prompt", "params":{"text":"hello"}});
    for request in [
        f.client.post(&f.base).header("accept", "application/json"),
        f.client.post(format!("{}/rejected-rpc", f.base)).json(&rpc),
        f.client
            .post(format!("{}/rejected-sse", f.base))
            .header("accept", "text/event-stream")
            .json(&json!({})),
        f.client.get(&f.base).header("accept", "application/json"),
    ] {
        // First present source fails closed even when a later cookie and default are valid.
        let response = request
            .header("x-user", "")
            .header("cookie", "owner=fallback")
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response
            .headers()
            .contains_key("access-control-allow-origin"));
        let body: Value = response.json().await?;
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert!(body["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()));
    }
    let response = f
        .client
        .post(&f.base)
        .header("accept", "application/json")
        .header("cookie", "owner=")
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(f.store.list().await?.is_empty());
    for session in ["rejected-rpc", "rejected-sse"] {
        assert!(!f
            .server
            .session_registry
            .has_session(&crate::session_actor::SessionKey::new(
                crate::session_actor::ResolvedAgentTarget::new("plain", CLUSTER),
                session
            )));
    }
    let preflight = f
        .client
        .request(reqwest::Method::OPTIONS, &f.base)
        .header("x-user", "")
        .send()
        .await?;
    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    f.handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn implicit_rpc_and_ag_ui_creation_keep_first_request_identity() -> Result<()> {
    let Some(f) = fixture(FixtureIdentity {
        sources: &["x-user", "cookie:owner"],
        cluster_user: Some("cluster-owner"),
        global_user: None,
    })
    .await?
    else {
        return Ok(());
    };
    f.prompt("implicit-rpc", Headers(&[("x-user", "first-owner")]))
        .await?;
    f.prompt("implicit-rpc", Headers(&[("x-user", "second-owner")]))
        .await?;
    assert_eq!(
        f.user_id("implicit-rpc").await?.as_deref(),
        Some("first-owner")
    );
    // A new session on the same server must use its own prompt, not a cached actor identity.
    f.prompt("other-rpc", Headers(&[("cookie", "owner=other-owner")]))
        .await?;
    assert_eq!(
        f.user_id("other-rpc").await?.as_deref(),
        Some("other-owner")
    );
    let response = f
        .client
        .post(format!("{}/implicit-sse", f.base))
        .header("accept", "text/event-stream")
        .header("x-user", "sse-owner")
        .json(&json!({
            "threadId": uuid::Uuid::new_v4(), "runId": uuid::Uuid::new_v4(),
            "messages": [{"id": uuid::Uuid::new_v4(), "role": "user", "content": "hello"}],
            "tools": [], "context": [], "forwardedProps": {}
        }))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    // AG-UI subscribes before admitting its prompt. Subscription must not create
    // default-owned metadata and prevent the prompt's identity from winning.
    assert_eq!(
        f.user_id("implicit-sse").await?.as_deref(),
        Some("sse-owner")
    );
    drop(response);
    f.prompt("implicit-sse", Headers(&[("x-user", "later-owner")]))
        .await?;
    assert_eq!(
        f.user_id("implicit-sse").await?.as_deref(),
        Some("sse-owner")
    );
    f.handle.shutdown().await?;
    Ok(())
}

#[test]
fn invalid_sources_fail_server_startup() {
    harnx_core::require_nextest();
    let sandbox = TestConfigSandbox::new();
    let mut config = sandbox.config();
    config.serve_user_id_sources = vec!["header:not a header".to_string()];
    let config = Arc::new(ConfigLock::new(config));
    let result = Server::new_with_stream_drain(
        &config,
        PathBuf::from("web-assets"),
        crate::StreamDrainConfig::default(),
    );
    let error = result.err().expect("startup rejects invalid source");
    assert!(format!("{error:#}").contains("invalid serve_user_id_sources"));
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_resume_does_not_create_default_owned_metadata_before_prompt() -> Result<()> {
    let Some(f) = fixture(FixtureIdentity {
        sources: &["x-user"],
        cluster_user: Some("cluster-owner"),
        global_user: None,
    })
    .await?
    else {
        return Ok(());
    };
    let resume =
        json!([{"interruptId":"missing-tool", "status":"resolved", "payload":{"approved":true}}]);
    let response: Value = f
        .client
        .post(format!("{}/resume-only", f.base))
        .header("x-user", "resume-owner")
        .json(&json!({
            "jsonrpc":"2.0", "id":1, "method":"session/prompt", "params":{"text":"", "resume":resume}
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(response["result"]["applied"], false);
    assert!(f
        .store
        .get_for_agent("resume-only", "plain")
        .await?
        .is_none());
    let response: Value = f.client.post(format!("{}/resume-and-prompt", f.base)).header("x-user", "prompt-owner").json(&json!({
        "jsonrpc":"2.0", "id":2, "method":"session/prompt", "params":{"text":"hello", "resume":resume}
    })).send().await?.error_for_status()?.json().await?;
    assert_eq!(response["result"]["status"], "accepted");
    assert_eq!(
        f.user_id("resume-and-prompt").await?.as_deref(),
        Some("prompt-owner")
    );
    f.handle.shutdown().await?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Attach {
    Subscribe,
    Resume,
}

impl std::fmt::Display for Attach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Subscribe => "subscribe",
            Self::Resume => "resume",
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Control {
    Cancel,
    Compact,
}

impl std::fmt::Display for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Cancel => "cancel",
            Self::Compact => "compact",
        })
    }
}

#[derive(Clone, Copy)]
struct Scenario {
    attach: Attach,
    control: Control,
}

const PROMPTLESS_SCENARIOS: [Scenario; 4] = [
    Scenario {
        attach: Attach::Subscribe,
        control: Control::Cancel,
    },
    Scenario {
        attach: Attach::Subscribe,
        control: Control::Compact,
    },
    Scenario {
        attach: Attach::Resume,
        control: Control::Cancel,
    },
    Scenario {
        attach: Attach::Resume,
        control: Control::Compact,
    },
];

struct PromptlessRequest<'a> {
    scenario: Scenario,
    session: &'a str,
    url: &'a str,
}

/// Test that promptless controls (subscribe/resume + cancel/compact) don't create metadata
/// until an actual prompt is submitted. Tests both subscribe and resume attach methods.
#[tokio::test(flavor = "multi_thread")]
async fn promptless_subscribe_and_resume_controls_do_not_create_metadata() -> Result<()> {
    let Some(f) = fixture(FixtureIdentity {
        sources: &["x-user"],
        cluster_user: Some("cluster-owner"),
        global_user: None,
    })
    .await?
    else {
        return Ok(());
    };

    for scenario in PROMPTLESS_SCENARIOS {
        test_promptless_control_path(&f, scenario).await?;
    }

    f.handle.shutdown().await?;
    Ok(())
}
/// Test a single combination of attach method and control method.
/// Verifies that metadata is not created until a prompt is submitted.
async fn test_promptless_control_path(f: &Fixture, scenario: Scenario) -> Result<()> {
    let session = format!("{}-{}", scenario.attach, scenario.control);
    let url = format!("{}/{}", f.base, session);
    let request = PromptlessRequest {
        scenario,
        session: &session,
        url: &url,
    };

    let subscription = attach_promptless_session(f, &request).await?;

    // Verify metadata is not created yet
    assert!(f.store.get_for_agent(&session, "plain").await?.is_none());

    // For compact, also test the actor guard path
    if scenario.control == Control::Compact {
        assert_promptless_compaction_rejected(f, &request).await?;
    }

    assert_promptless_control_response(f, &request).await?;

    // Verify metadata still not created, then submit prompt
    assert!(f.store.get_for_agent(&session, "plain").await?.is_none());
    f.prompt(&session, Headers(&[("x-user", "prompt-owner")]))
        .await?;
    assert_eq!(f.user_id(&session).await?.as_deref(), Some("prompt-owner"));

    drop(subscription);
    Ok(())
}

async fn attach_promptless_session(
    f: &Fixture,
    request: &PromptlessRequest<'_>,
) -> Result<Option<reqwest::Response>> {
    let url = request.url;
    let subscription =
        if request.scenario.attach == Attach::Subscribe {
            let response = f
                .client
                .post(url)
                .header("accept", "text/event-stream")
                .header("x-user", "attach-owner")
                .json(&json!({
                    "threadId": uuid::Uuid::new_v4(), "runId": uuid::Uuid::new_v4(), "messages": [],
                    "tools": [], "context": [], "forwardedProps": {}
                }))
                .send()
                .await?;
            assert_eq!(response.status(), StatusCode::OK);
            Some(response)
        } else {
            let response: Value = f.client.post(url).header("x-user", "resume-owner").json(&json!({
            "jsonrpc":"2.0", "id":1, "method":"session/prompt", "params":{"text":"", "resume":[
                {"interruptId":"missing-tool", "status":"resolved", "payload":{"approved":true}}
            ]}
        })).send().await?.error_for_status()?.json().await?;
            assert_eq!(response["result"]["applied"], false);
            None
        };
    Ok(subscription)
}

async fn assert_promptless_compaction_rejected(
    f: &Fixture,
    request: &PromptlessRequest<'_>,
) -> Result<()> {
    let session = request.session;
    let handle = f
        .server
        .session_registry
        .get_or_spawn(crate::session_actor::SessionKey::new(
            crate::session_actor::ResolvedAgentTarget::new("plain", CLUSTER),
            session,
        ));
    let (reply, response) = tokio::sync::oneshot::channel();
    handle
        .tx
        .send(crate::session_actor::SessionCommand::Compact { reply })
        .await
        .expect("compact command sent");
    assert_eq!(response.await?.unwrap_err(), "session not found");
    Ok(())
}

async fn assert_promptless_control_response(
    f: &Fixture,
    request: &PromptlessRequest<'_>,
) -> Result<()> {
    let url = request.url;
    let method = request.scenario.control;
    let method_full = format!("session/{method}");
    let response = f
        .client
        .post(url)
        .header("x-user", "control-owner")
        .json(&json!({"jsonrpc":"2.0", "id":2, "method":method_full}))
        .send()
        .await?;

    let expected_status = if method == Control::Cancel {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    assert_eq!(response.status(), expected_status);

    let body: Value = response.json().await?;
    if method == Control::Cancel {
        assert_eq!(
            body["result"],
            serde_json::to_value(harnx_runtime::nats_session::InterruptOutcome::Idle)?
        );
    } else {
        assert_eq!(
            body["error"]["code"],
            crate::ag_ui_rpc::JSON_RPC_UNKNOWN_SESSION_CODE
        );
    }
    Ok(())
}

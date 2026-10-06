use super::*;
use harnx_serve::session_actor::{ResolvedAgentTarget, SessionKey};

async fn assert_prompt_refused(config: &Config, registry: &SessionRegistry) {
    let refused = handle_ag_ui_rpc_bytes(
        Method::POST,
        "plain@prompt-test",
        &format!("rpc-refused-{}", Uuid::new_v4()),
        Bytes::from(json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"text":"bad attachment","attachment_refs":["cid:missing"]}}).to_string()),
        config,
        registry,
        PersistenceKind::Nats,
    )
    .await
    .expect("refused prompt response");
    assert_eq!(refused.status(), http::StatusCode::CONFLICT);
    let refused_body = refused
        .into_body()
        .collect()
        .await
        .expect("refused body")
        .to_bytes();
    let refused: Value = serde_json::from_slice(&refused_body).expect("refused JSON");
    assert_eq!(refused["error"]["code"], -32004);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_prompt_creates_session_visible_to_another_registry() {
    harnx_core::require_nextest();
    let _guard = TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    let Some(nats) = common::spawn_nats_server().await.expect("test NATS server") else {
        return;
    };
    // A remote cluster exercises durable admission without starting a local worker.
    sandbox.write_nats_server("prompt-test", &format!("url: {:?}\n", nats.url()));
    let config = sandbox.config();
    let registry = SessionRegistry::new(config.clone());
    let target = ResolvedAgentTarget::new("plain", "prompt-test");
    let session_id = format!("rpc-create-{}", Uuid::new_v4());
    let key = SessionKey::new(target, &session_id);
    assert!(!registry.has_session(&key));

    for method in [
        "session/get",
        "session/cancel",
        "session/compact",
        "session/mark_read",
        "session/mark_unread",
    ] {
        let response = rpc_call(
            &config,
            &registry,
            "plain@prompt-test",
            &session_id,
            json!({"jsonrpc":"2.0","id":1,"method":method}),
        )
        .await;
        assert_eq!(response["error"]["code"], -32001, "{method}: {response}");
        assert!(
            !registry.has_session(&key),
            "{method} must not create a session"
        );
    }

    let prompt = rpc_call(
        &config,
        &registry,
        "plain@prompt-test",
        &session_id,
        json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"text":"create through RPC"}}),
    )
    .await;
    assert_eq!(prompt["result"]["status"], "accepted", "{prompt}");
    assert!(prompt["result"]["run_id"].as_str().is_some());
    assert!(registry.has_session(&key));
    assert_prompt_refused(&config, &registry).await;

    // A fresh registry proves existence comes from NATS, not the first actor's cache.
    let other_registry = SessionRegistry::new(config.clone());
    let session = rpc_call(
        &config,
        &other_registry,
        "plain@prompt-test",
        &session_id,
        json!({"jsonrpc":"2.0","id":3,"method":"session/get"}),
    )
    .await;
    assert!(session["error"].is_null(), "{session}");
    assert_eq!(session["result"]["capabilities"]["persistence"], "nats");
    assert!(history_snapshot_texts(&session["result"]).contains(&"create through RPC".to_string()));
}

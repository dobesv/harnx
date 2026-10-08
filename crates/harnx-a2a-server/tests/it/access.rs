//! Rules-on HTTP checks against isolated JetStream and a real worker.
use crate::{
    support::{Harness, Script},
    unary::{message, Http, HttpRpcCall},
};
use anyhow::Result;
use harnx_a2a_server::handler::{HarnxHandler, PERMISSION_DENIED_CODE};
use harnx_core::{access_rules::AccessRules, session_identity::session_key};
use serde_json::{json, Value};
use std::sync::Arc;

fn rules() -> Arc<AccessRules> {
    Arc::new(
        AccessRules::from_yaml(
            r#"
rules:
  - agents: [runner@runner]
    users: [alice, bob]
  - agents: [runner@runner]
    scopes: [admin]
    users: [admin]
  - agents: [public@runner]
    users: [alias-only]
"#,
        )
        .unwrap(),
    )
}

async fn start(enabled: bool) -> Result<Http> {
    let rules = enabled.then(rules);
    let mut h = Harness::start_with_access_rules(Script::Text, rules.clone()).await?;
    // Same export shape as --agent public=runner --cluster runner.
    h.export.public_name = "public".into();
    Http::with_access_rules(h, rules).await
}

async fn rpc(http: &Http, user: &str, method: &str, params: Value) -> Result<Value> {
    http.rpc_call(HttpRpcCall::new(&http.url, user, method, params))
        .await
}

fn code(response: &Value, expected: i32) {
    assert_eq!(response["error"]["code"], expected, "{response}");
    assert!(response.get("result").is_none(), "{response}");
}

async fn send(http: &Http, user: &str, id: &str, context: Option<&Value>) -> Result<Value> {
    http.h.llm.release.notify_one();
    let response = rpc(
        http,
        user,
        "SendMessage",
        json!({"message": message(id, context)}),
    )
    .await?;
    assert!(response["error"].is_null(), "{response}");
    let task = response["result"]["task"].clone();
    assert_eq!(
        task["status"]["state"], "TASK_STATE_COMPLETED",
        "{response}"
    );
    Ok(task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_nats_hidden_export_and_internal_alias() -> Result<()> {
    let http = start(true).await?;
    let root = http.url.strip_suffix("/runner").unwrap();
    for path in card_paths(&http, &["runner", "alias", "public"]) {
        let response = http
            .client
            .get(&path)
            .header("X-User-ID", "alice")
            .send()
            .await?;
        assert_eq!(response.status(), 200, "{path}");
        let card: Value = response.json().await?;
        assert!(card["supportedInterfaces"][0]["url"]
            .as_str()
            .unwrap()
            .ends_with("/agents/public"));
        // Granting scopes on the public alias must not grant the internal agent.
        let response = http
            .client
            .get(&path)
            .header("X-User-ID", "alias-only")
            .send()
            .await?;
        assert_eq!(response.status(), 404, "{path}");
        assert!(response.bytes().await?.is_empty());
    }
    assert_hidden_rpc_routes(&http).await?;
    let response = http
        .client
        .get(format!("{root}/other/.well-known/agent-card.json"))
        .header("X-User-ID", "alice")
        .send()
        .await?;
    assert_eq!(response.status(), 404);
    assert!(http.h.metadata.list().await?.is_empty());
    assert!(http.h.llm.requests.lock().is_empty());
    Ok(())
}

async fn assert_hidden_rpc_routes(http: &Http) -> Result<()> {
    let root = http.url.strip_suffix("/runner").unwrap();
    for (name, user) in [
        ("other", "alice"),
        ("other", "admin"),
        ("public", "alias-only"),
    ] {
        for method in [
            "SendMessage",
            "SendStreamingMessage",
            "GetTask",
            "CancelTask",
            "SubscribeToTask",
            "ListTasks",
            "GetExtendedAgentCard",
            "CreateTaskPushNotificationConfig",
            "UnknownMethod",
            "tasks/get",
        ] {
            for suffix in ["", "/"] {
                let response = http
                    .client
                    .post(format!("{root}/{name}{suffix}"))
                    .header("X-User-ID", user)
                    // Unsupported version and invalid params prove hiding precedes dispatch.
                    .header("a2a-version", "99")
                    .json(&json!({"jsonrpc":"2.0","id":"hidden","method":method,"params":{}}))
                    .send()
                    .await?;
                assert_eq!(response.status(), 200);
                let response: Value = response.json().await?;
                assert_eq!(response["id"], "hidden");
                code(&response, -32001);
                assert_eq!(response["error"]["message"], "task not found");
            }
        }
    }
    Ok(())
}

fn card_paths(http: &Http, names: &[&str]) -> Vec<String> {
    let root = http.url.strip_suffix("/runner").unwrap();
    names
        .iter()
        .flat_map(|name| {
            ["agent-card.json", "agent.json"]
                .map(|suffix| format!("{root}/{name}/.well-known/{suffix}"))
        })
        .collect()
}

fn card_request(http: &Http, path: &str, identity: Option<&str>) -> reqwest::RequestBuilder {
    let request = http.client.get(path);
    match identity {
        Some(identity) => request.header("X-User-ID", identity),
        None => request,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_nats_prompt_create_continue_and_foreign_context_hidden() -> Result<()> {
    let http = start(true).await?;
    let first = send(&http, "alice", "first", None).await?;
    let context = &first["contextId"];
    let own = send(&http, "alice", "follow-up", Some(context)).await?;
    assert_eq!(own["contextId"], *context);
    assert_ne!(own["id"], first["id"]);
    let key = session_key(Some("runner"), context.as_str().unwrap());
    let binding = http.h.store.get_binding(&key).await?.unwrap();
    assert_eq!(binding.owner.as_deref(), Some("alice"));
    for method in [
        "SendMessage",
        "SendStreamingMessage",
        "GetTask",
        "CancelTask",
        "SubscribeToTask",
        "ListTasks",
    ] {
        let params = match method {
            "SendMessage" | "SendStreamingMessage" => {
                json!({"message": message("foreign", Some(context))})
            }
            "ListTasks" => json!({"contextId":context}),
            _ => json!({"id":first["id"]}),
        };
        code(&rpc(&http, "bob", method, params).await?, -32001);
    }
    // Dedupe and taskId-derived context authorization must not bypass ownership.
    let mut retry = message("first", Some(context));
    retry["taskId"] = first["id"].clone();
    code(
        &rpc(&http, "bob", "SendMessage", json!({"message":retry})).await?,
        -32001,
    );
    let listing = rpc(&http, "alice", "ListTasks", json!({"contextId":context})).await?;
    assert_eq!(listing["result"]["tasks"].as_array().unwrap().len(), 2);
    let missing = json!("unknown-context");
    for user in ["alice", "admin"] {
        code(
            &rpc(
                &http,
                user,
                "SendMessage",
                json!({"message":message("unknown", Some(&missing))}),
            )
            .await?,
            -32001,
        );
    }
    assert_eq!(http.h.metadata.list().await?.len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_nats_admin_reads_lists_continues_without_taking_ownership() -> Result<()> {
    let http = start(true).await?;
    let alice = send(&http, "alice", "alice-first", None).await?;
    let bob = send(&http, "bob", "bob-first", None).await?;
    for (original, first) in [("alice", &alice), ("bob", &bob)] {
        let read = rpc(&http, "admin", "GetTask", json!({"id":first["id"]})).await?;
        assert_eq!(read["result"]["id"], first["id"]);
        let context = &first["contextId"];
        let task = send(&http, "admin", "admin-follow-up", Some(context)).await?;
        assert_eq!(task["contextId"], *context);
        let listing = rpc(&http, "admin", "ListTasks", json!({"contextId":context})).await?;
        assert_eq!(listing["result"]["tasks"].as_array().unwrap().len(), 2);
        let key = session_key(Some("runner"), context.as_str().unwrap());
        assert_eq!(
            http.h
                .store
                .get_binding(&key)
                .await?
                .unwrap()
                .owner
                .as_deref(),
            Some(original)
        );
        let record = http.h.metadata.get(&key).await?.unwrap();
        assert_eq!(
            harnx_runtime::nats_session_metadata::session_properties(&record.metadata)?
                .text("user_id"),
            Some(original)
        );
        // The original owner can still operate after the admin's turn.
        assert!(
            rpc(&http, original, "GetTask", json!({"id":task["id"]})).await?["error"].is_null()
        );
    }
    code(
        &rpc(
            &http,
            "alice",
            "ListTasks",
            json!({"contextId":bob["contextId"]}),
        )
        .await?,
        -32001,
    );
    // Admin privileges don't cross the export component of the durable binding.
    let mut wrong = http.h.export.clone();
    wrong.public_name = "different-export".into();
    assert!(http
        .h
        .store
        .resolve_context(
            &wrong,
            &harnx_a2a_server::identity::Principal::User("admin".into()),
            alice["contextId"].as_str().unwrap()
        )
        .await?
        .is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_nats_admin_new_context_permission_error() -> Result<()> {
    let http = start(true).await?;
    for method in [
        "SendMessage",
        "SendStreamingMessage",
        "message/send",
        "message/stream",
    ] {
        let response = rpc(
            &http,
            "admin",
            method,
            json!({"message":message(method, None)}),
        )
        .await?;
        code(&response, PERMISSION_DENIED_CODE);
        assert_eq!(
            response["error"]["message"],
            "session creation requires prompt scope"
        );
    }
    assert!(http.h.metadata.list().await?.is_empty());
    assert!(http.h.llm.requests.lock().is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_nats_cards_require_identity_only_with_rules() -> Result<()> {
    let http = start(true).await?;
    for path in card_paths(&http, &["runner", "public", "other"]) {
        for identity in [None, Some(""), Some("  ")] {
            let response = card_request(&http, &path, identity).send().await?;
            assert_eq!(response.status(), 401);
            code(&response.json().await?, -32000);
        }
    }
    // Anonymous mode cannot accidentally enable access rules.
    let error = harnx_a2a_server::routes::router_with_access_rules::<HarnxHandler>(
        &[],
        None,
        &[],
        Some(rules()),
        |_, _| unreachable!(),
    );
    assert!(error.is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_nats_rules_off_preserves_public_cards_and_strict_ownership() -> Result<()> {
    let http = start(false).await?;
    for path in card_paths(&http, &["runner", "public", "other"]) {
        let response = card_request(&http, &path, Some("")).send().await?;
        assert_eq!(response.status(), 200);
    }
    let first = send(&http, "alice", "first", None).await?;
    code(
        &rpc(&http, "admin", "GetTask", json!({"id":first["id"]})).await?,
        -32001,
    );
    code(
        &rpc(
            &http,
            "bob",
            "ListTasks",
            json!({"contextId":first["contextId"]}),
        )
        .await?,
        -32001,
    );
    // No access policy: the caller named admin can create like any other user.
    let own = send(&http, "admin", "admin-first", None).await?;
    send(&http, "admin", "admin-next", Some(&own["contextId"])).await?;
    Ok(())
}

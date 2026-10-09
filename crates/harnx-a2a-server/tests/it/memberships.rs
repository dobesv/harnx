//! Request-local memberships on HTTP admission, task guards and streaming.
use crate::{
    streaming::{until_artifact, Frames},
    support::{Harness, Script},
    unary::{message, Http},
};
use anyhow::Result;
use harnx_a2a_server::{handler::PERMISSION_DENIED_CODE, identity::Identity};
use harnx_core::{access_rules::AccessRules, session_identity::session_key};
use reqwest::{
    header::{HeaderMap, HeaderValue},
    RequestBuilder,
};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone, Copy)]
struct TestCaller<'a> {
    user: &'a str,
    group: &'a str,
    role: &'a str,
}

fn caller<'a>(user: &'a str, group: &'a str, role: &'a str) -> TestCaller<'a> {
    TestCaller { user, group, role }
}

fn rules() -> Arc<AccessRules> {
    Arc::new(
        AccessRules::from_yaml(
            r#"
rules:
  - agents: [runner@runner]
    groups: [team]
  - agents: [runner@runner]
    roles: [reviewer]
  - agents: [runner@runner]
    users: [user-grant]
  - agents: [runner@runner]
    scopes: [admin]
    groups: [admins]
  - agents: [runner@runner]
    scopes: [admin]
    roles: [supervisor]
"#,
        )
        .unwrap(),
    )
}

async fn start(enabled: bool, user_required: bool) -> Result<Http> {
    let rules = enabled.then(rules);
    let h = Harness::start_with_access_rules(Script::Text, rules.clone()).await?;
    let users = if user_required {
        vec!["X-User-ID".into()]
    } else {
        vec![]
    };
    let policy = Identity::with_memberships(
        &users,
        &["X-Groups".into(), "X-Other-Groups".into()],
        &["X-Roles".into(), "X-Other-Roles".into()],
    )?;
    Http::with_identity(h, rules, policy).await
}

fn request(http: &Http, user: Option<&str>, method: &str, params: Value) -> RequestBuilder {
    let request = http
        .client
        .post(&http.url)
        .header("a2a-version", "1.0")
        .json(&json!({"jsonrpc":"2.0","id":"stream-request","method":method,"params":params}));
    match user {
        Some(user) => request.header("X-User-ID", user),
        None => request,
    }
}

fn memberships(request: RequestBuilder, group: &str, role: &str) -> RequestBuilder {
    request.header("X-Groups", group).header("X-Roles", role)
}

async fn rpc(http: &Http, identity: TestCaller<'_>, method: &str, params: Value) -> Result<Value> {
    let response = memberships(
        request(http, Some(identity.user), method, params),
        identity.group,
        identity.role,
    )
    .send()
    .await?;
    assert_eq!(response.status(), 200);
    Ok(response.json().await?)
}

fn code(response: &Value, expected: i32) {
    assert_eq!(response["error"]["code"], expected, "{response}");
    assert!(response.get("result").is_none(), "{response}");
}

async fn send(
    http: &Http,
    identity: TestCaller<'_>,
    id: &str,
    context: Option<&Value>,
) -> Result<Value> {
    http.h.llm.release.notify_one();
    let response = rpc(
        http,
        identity,
        "SendMessage",
        json!({"message":message(id, context)}),
    )
    .await?;
    assert!(response["error"].is_null(), "{response}");
    let task = response["result"]["task"].clone();
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
    Ok(task)
}

async fn assert_owner(http: &Http, task: &Value, expected: Option<&str>) -> Result<()> {
    let key = session_key(Some("runner"), task["contextId"].as_str().unwrap());
    assert_eq!(
        http.h
            .store
            .get_binding(&key)
            .await?
            .unwrap()
            .owner
            .as_deref(),
        expected
    );
    let record = http.h.metadata.get(&key).await?.unwrap();
    assert_eq!(
        harnx_runtime::nats_session_metadata::session_properties(&record.metadata)?.text("user_id"),
        expected
    );
    let metadata = serde_json::to_string(&record.metadata)?;
    for membership in ["team", "reviewer", "admins", "supervisor"] {
        assert!(
            !metadata.contains(membership),
            "membership persisted: {metadata}"
        );
    }
    Ok(())
}

async fn card(http: &Http, identity: TestCaller<'_>) -> Result<u16> {
    Ok(memberships(
        http.client
            .get(format!("{}/.well-known/agent-card.json", http.url))
            .header("X-User-ID", identity.user),
        identity.group,
        identity.role,
    )
    .send()
    .await?
    .status()
    .as_u16())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memberships_group_and_role_only_collect_all_headers_and_preserve_owner() -> Result<()> {
    let http = start(true, true).await?;
    for (name, other) in [("X-Groups", "X-Other-Groups"), ("X-Roles", "X-Other-Roles")] {
        let grant = if name == "X-Groups" {
            "team"
        } else {
            "reviewer"
        };
        let mut headers = HeaderMap::new();
        headers.append(name, HeaderValue::from_static(" ,ignored, ,"));
        headers.append(name, HeaderValue::from_static("duplicate"));
        headers.append(other, grant.parse()?);
        headers.append(other, grant.parse()?);
        for suffix in ["agent-card.json", "agent.json"] {
            let response = http
                .client
                .get(format!("{}/.well-known/{suffix}", http.url))
                .header("X-User-ID", "alice")
                .headers(headers.clone())
                .send()
                .await?;
            assert_eq!(response.status(), 200);
        }
        http.h.llm.release.notify_one();
        let response = request(
            &http,
            Some("alice"),
            "SendMessage",
            json!({"message":message(name, None)}),
        )
        .headers(headers)
        .send()
        .await?;
        assert_eq!(response.status(), 200);
        let response: Value = response.json().await?;
        assert!(response["error"].is_null(), "{response}");
        let task = &response["result"]["task"];
        assert_owner(&http, task, Some("alice")).await?;
        let read = rpc(
            &http,
            caller("alice", "team", ""),
            "GetTask",
            json!({"id":task["id"]}),
        )
        .await?;
        assert_eq!(read["result"]["id"], task["id"]);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memberships_namespaces_do_not_collide_or_use_unconfigured_headers() -> Result<()> {
    let http = start(true, true).await?;
    for (user, group, role) in [
        ("team", "", ""),
        ("reviewer", "", ""),
        ("alice", "reviewer", "team"),
        ("alice", "user-grant", "user-grant"),
        ("supervisor", "", ""),
    ] {
        assert_eq!(card(&http, caller(user, group, role)).await?, 404);
        code(
            &rpc(
                &http,
                caller(user, group, role),
                "SendMessage",
                json!({"message":message("collision", None)}),
            )
            .await?,
            -32001,
        );
    }
    let response = http
        .client
        .get(format!("{}/.well-known/agent-card.json", http.url))
        .header("X-User-ID", "alice")
        .header("X-Unconfigured-Groups", "team")
        .header("X-Unconfigured-Roles", "supervisor")
        .send()
        .await?;
    assert_eq!(response.status(), 404);
    assert_eq!(card(&http, caller("user-grant", "", "")).await?, 200);
    assert!(http.h.metadata.list().await?.is_empty());
    assert!(http.h.llm.requests.lock().is_empty());
    Ok(())
}

fn task_params(method: &str, task: &Value) -> Value {
    match method {
        "SendMessage" | "SendStreamingMessage" => {
            json!({"message":message("follow-up", Some(&task["contextId"]))})
        }
        "ListTasks" => json!({"contextId":task["contextId"]}),
        _ => json!({"id":task["id"]}),
    }
}

async fn assert_hidden_tasks(http: &Http, identity: TestCaller<'_>, task: &Value) -> Result<()> {
    for method in [
        "SendMessage",
        "SendStreamingMessage",
        "GetTask",
        "ListTasks",
        "CancelTask",
        "SubscribeToTask",
    ] {
        code(
            &rpc(http, identity, method, task_params(method, task)).await?,
            -32001,
        );
    }
    let mut retry = message("first", Some(&task["contextId"]));
    retry["taskId"] = task["id"].clone();
    code(
        &rpc(http, identity, "SendMessage", json!({"message":retry})).await?,
        -32001,
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memberships_next_request_removal_changes_and_foreign_ownership() -> Result<()> {
    let http = start(true, true).await?;
    let first = send(&http, caller("alice", "team", ""), "first", None).await?;
    // Removed or changed memberships affect every subsequent request, including retries.
    for (group, role) in [("", ""), ("other-team", "other-role")] {
        assert_eq!(card(&http, caller("alice", group, role)).await?, 404);
        assert_hidden_tasks(&http, caller("alice", group, role), &first).await?;
    }
    // A different prompt member sees the export but never acquires Alice's task.
    assert_eq!(card(&http, caller("bob", "team", "reviewer")).await?, 200);
    assert_hidden_tasks(&http, caller("bob", "team", "reviewer"), &first).await?;
    let restored = rpc(
        &http,
        caller("alice", "", "reviewer"),
        "GetTask",
        json!({"id":first["id"]}),
    )
    .await?;
    assert_eq!(restored["result"]["id"], first["id"]);
    let mut retry = stream(
        &http,
        caller("alice", "", "reviewer"),
        "SendStreamingMessage",
        json!({"message":message("first", None)}),
    )
    .await?;
    assert_eq!(retry.event().await?.unwrap()["task"]["id"], first["id"]);
    assert!(retry.event().await?.is_none());
    let next = send(
        &http,
        caller("alice", "", "reviewer"),
        "next",
        Some(&first["contextId"]),
    )
    .await?;
    assert_eq!(next["contextId"], first["contextId"]);
    let list = rpc(
        &http,
        caller("alice", "", "reviewer"),
        "ListTasks",
        json!({"contextId":first["contextId"]}),
    )
    .await?;
    assert_eq!(list["result"]["tasks"].as_array().unwrap().len(), 2);
    assert_owner(&http, &first, Some("alice")).await?;
    assert_eq!(http.h.metadata.list().await?.len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memberships_admin_override_and_scope_union_do_not_transfer_ownership() -> Result<()> {
    let http = start(true, true).await?;
    let first = send(&http, caller("alice", "team", ""), "first", None).await?;
    for (group, role) in [("admins", ""), ("", "supervisor")] {
        code(
            &rpc(
                &http,
                caller("bob", group, role),
                "SendMessage",
                json!({"message":message("admin-new", None)}),
            )
            .await?,
            PERMISSION_DENIED_CODE,
        );
        let read = rpc(
            &http,
            caller("bob", group, role),
            "GetTask",
            json!({"id":first["id"]}),
        )
        .await?;
        assert_eq!(read["result"]["id"], first["id"]);
        let list = rpc(
            &http,
            caller("bob", group, role),
            "ListTasks",
            json!({"contextId":first["contextId"]}),
        )
        .await?;
        assert!(!list["result"]["tasks"].as_array().unwrap().is_empty());
        let continued = send(
            &http,
            caller("bob", group, role),
            &format!("{group}-{role}"),
            Some(&first["contextId"]),
        )
        .await?;
        assert_eq!(continued["contextId"], first["contextId"]);
        assert_owner(&http, &continued, Some("alice")).await?;
    }
    // Separate rules union admin and prompt; user grants can participate too.
    let own = send(&http, caller("bob", "admins", "reviewer"), "union", None).await?;
    assert_owner(&http, &own, Some("bob")).await?;
    let user_union = send(
        &http,
        caller("user-grant", "", "supervisor"),
        "user-union",
        None,
    )
    .await?;
    assert_owner(&http, &user_union, Some("user-grant")).await?;
    assert_hidden_tasks(&http, caller("bob", "team", ""), &first).await?;
    Ok(())
}

async fn stream(
    http: &Http,
    identity: TestCaller<'_>,
    method: &str,
    params: Value,
) -> Result<Frames> {
    Frames::from_response(
        memberships(
            request(http, Some(identity.user), method, params),
            identity.group,
            identity.role,
        )
        .send()
        .await?,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memberships_stream_resubscribe_cancel_and_revocation() -> Result<()> {
    let http = start(true, true).await?;
    let mut frames = stream(
        &http,
        caller("alice", "team", ""),
        "SendStreamingMessage",
        json!({"message":message("stream", None)}),
    )
    .await?;
    let first = frames.event().await?.unwrap()["task"].clone();
    until_artifact(&mut frames, &mut String::new()).await?;
    drop(frames);
    assert_hidden_tasks(&http, caller("alice", "", ""), &first).await?;
    assert_hidden_tasks(&http, caller("bob", "team", "reviewer"), &first).await?;
    let mut restored = stream(
        &http,
        caller("alice", "", "reviewer"),
        "SubscribeToTask",
        json!({"id":first["id"]}),
    )
    .await?;
    assert_eq!(restored.event().await?.unwrap()["task"]["id"], first["id"]);
    drop(restored);
    let mut admin = stream(
        &http,
        caller("bob", "", "supervisor"),
        "SubscribeToTask",
        json!({"id":first["id"]}),
    )
    .await?;
    assert_eq!(admin.event().await?.unwrap()["task"]["id"], first["id"]);
    drop(admin);
    let canceled = rpc(
        &http,
        caller("bob", "admins", ""),
        "CancelTask",
        json!({"id":first["id"]}),
    )
    .await?;
    assert_eq!(
        canceled["result"]["status"]["state"], "TASK_STATE_CANCELED",
        "{canceled}"
    );
    assert_owner(&http, &first, Some("alice")).await?;
    let read = rpc(
        &http,
        caller("alice", "team", ""),
        "GetTask",
        json!({"id":first["id"]}),
    )
    .await?;
    assert_eq!(read["result"]["status"]["state"], "TASK_STATE_CANCELED");
    assert_eq!(http.h.llm.requests.lock().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memberships_missing_user_basic_bearer_and_malformed_later_headers() -> Result<()> {
    let http = start(true, true).await?;
    for authorization in ["Basic YWxpY2U6c2VjcmV0", "Bearer secret"] {
        let response = memberships(
            request(&http, None, "GetTask", json!({"id":"unknown"})),
            "team",
            "supervisor",
        )
        .header("Authorization", authorization)
        .send()
        .await?;
        assert_eq!(response.status(), 401);
        code(&response.json().await?, -32000);
        let response = http
            .client
            .get(format!("{}/.well-known/agent-card.json", http.url))
            .header("Authorization", authorization)
            .header("X-Groups", "team")
            .send()
            .await?;
        assert_eq!(response.status(), 401);
    }
    for name in ["X-Groups", "X-Other-Groups", "X-Roles", "X-Other-Roles"] {
        let mut headers = HeaderMap::new();
        headers.append(name, HeaderValue::from_static("team,reviewer"));
        headers.append(name, HeaderValue::from_bytes(b"secret-membership\xff")?);
        for card in [false, true] {
            let request = if card {
                http.client
                    .get(format!("{}/.well-known/agent-card.json", http.url))
                    .header("X-User-ID", "alice")
            } else {
                request(
                    &http,
                    Some("alice"),
                    "SendMessage",
                    json!({"message":message("malformed", None)}),
                )
            };
            let response = request.headers(headers.clone()).send().await?;
            assert_eq!(response.status(), 401);
            let body = response.text().await?;
            assert!(!body.contains("secret-membership"));
            let body: Value = serde_json::from_str(&body)?;
            code(&body, -32000);
        }
    }
    assert!(http.h.metadata.list().await?.is_empty());
    assert!(http.h.llm.requests.lock().is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memberships_rules_off_keeps_public_discovery_and_ownership_modes() -> Result<()> {
    let isolated = start(false, true).await?;
    assert_eq!(card(&isolated, caller("", "", "")).await?, 200);
    let first = send(&isolated, caller("alice", "team", ""), "first", None).await?;
    let read = rpc(
        &isolated,
        caller("alice", "", ""),
        "GetTask",
        json!({"id":first["id"]}),
    )
    .await?;
    assert_eq!(read["result"]["id"], first["id"]);
    code(
        &rpc(
            &isolated,
            caller("bob", "admins", "supervisor"),
            "GetTask",
            json!({"id":first["id"]}),
        )
        .await?,
        -32001,
    );
    assert_owner(&isolated, &first, Some("alice")).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memberships_rules_off_shared_anonymous_mode_ignores_auth_and_owner_headers() -> Result<()>
{
    let anonymous = start(false, false).await?;
    anonymous.h.llm.release.notify_one();
    let response = memberships(
        request(
            &anonymous,
            None,
            "SendMessage",
            json!({"message":message("anon", None)}),
        ),
        "team",
        "supervisor",
    )
    .header("Authorization", "Bearer ignored")
    .send()
    .await?;
    assert_eq!(response.status(), 200);
    let response: Value = response.json().await?;
    assert!(response["error"].is_null(), "{response}");
    let task = &response["result"]["task"];
    assert_owner(&anonymous, task, None).await?;
    let read = rpc(
        &anonymous,
        caller("ignored-user", "", ""),
        "GetTask",
        json!({"id":task["id"]}),
    )
    .await?;
    assert_eq!(read["result"]["id"], task["id"]);
    Ok(())
}

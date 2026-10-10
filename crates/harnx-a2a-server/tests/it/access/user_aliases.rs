//! Caller aliases through real JSON-RPC admission, task access and worker resume.
use super::{code, rpc, send};
use crate::{
    support::{Harness, Script},
    unary::{message, Http},
};
use anyhow::Result;
use harnx_a2a_server::identity::Identity;
use harnx_core::{
    access_rules::AccessRules, session_identity::session_key, user_aliases::UserAliases,
};
use serde_json::{json, Value};
use std::sync::Arc;

fn aliases() -> Arc<UserAliases> {
    Arc::new(UserAliases::from_yaml("- name: Display\n  identities: [alice, bob, bob]\n- name: Overlap\n  identities: [bob, carol]\n").unwrap())
}

async fn start(enabled: bool) -> Result<Http> {
    let rules = enabled.then(|| {
        Arc::new(AccessRules::from_yaml(
        "rules:\n  - agents: [runner@runner]\n    users: [bob, carol, unknown, unrelated]\n"
    ).unwrap())
    });
    let h = Harness::start_with_access_rules(Script::Text, rules.clone()).await?;
    Http::with_identity(
        h,
        rules,
        Identity::new(&["X-User-ID".into()])?.with_user_aliases(Some(aliases())),
    )
    .await
}

async fn assert_raw_owner(http: &Http, context: &Value, owner: Option<&str>) -> Result<()> {
    let key = session_key(Some("runner"), context.as_str().unwrap());
    assert_eq!(
        http.h
            .store
            .get_binding(&key)
            .await?
            .unwrap()
            .owner
            .as_deref(),
        owner
    );
    let record = http.h.metadata.get(&key).await?.unwrap();
    assert_eq!(
        harnx_runtime::nats_session_metadata::session_properties(&record.metadata)?.text("user_id"),
        owner
    );
    Ok(())
}

async fn assert_hidden_context(http: &Http, caller: &str, task: &Value) -> Result<()> {
    for (method, params) in [
        ("GetTask", json!({"id":task["id"]})),
        ("CancelTask", json!({"id":task["id"]})),
        ("SubscribeToTask", json!({"id":task["id"]})),
        ("ListTasks", json!({"contextId":task["contextId"]})),
        (
            "SendMessage",
            json!({"message":message("denied", Some(&task["contextId"]))}),
        ),
        (
            "SendStreamingMessage",
            json!({"message":message("denied-stream", Some(&task["contextId"]))}),
        ),
    ] {
        code(&rpc(http, caller, method, params).await?, -32001);
    }
    Ok(())
}

async fn alias_client_paths(enabled: bool) -> Result<()> {
    let http = start(enabled).await?;
    let first = send(&http, "bob", "first", None).await?;
    let context = &first["contextId"];
    let read = rpc(&http, "alice", "GetTask", json!({"id":first["id"]})).await?;
    assert_eq!(read["result"]["id"], first["id"], "{read}");
    let followup = send(&http, "alice", "alias-followup", Some(context)).await?;
    assert_eq!(followup["contextId"], *context);
    let listing = rpc(&http, "alice", "ListTasks", json!({"contextId":context})).await?;
    assert_eq!(
        listing["result"]["tasks"].as_array().unwrap().len(),
        2,
        "{listing}"
    );

    let working = rpc(
        &http,
        "bob",
        "SendMessage",
        json!({
            "message":message("cancel-through-alias", Some(context)),
            "configuration":{"returnImmediately":true}
        }),
    )
    .await?;
    assert!(working["error"].is_null(), "{working}");
    let active_id = &working["result"]["task"]["id"];
    let cancel = rpc(&http, "alice", "CancelTask", json!({"id":active_id})).await?;
    assert_eq!(
        cancel["result"]["status"]["state"], "TASK_STATE_CANCELED",
        "{cancel}"
    );
    let read = rpc(&http, "alice", "GetTask", json!({"id":active_id})).await?;
    assert_eq!(read["result"]["status"]["state"], "TASK_STATE_CANCELED");
    assert_raw_owner(&http, context, Some("bob")).await?;

    // Alice has no direct rule grant. Admission must use Bob from her aliases.
    let incoming = send(&http, "alice", "new-as-alice", None).await?;
    assert_raw_owner(&http, &incoming["contextId"], Some("alice")).await?;
    let unrelated = send(&http, "unrelated", "new-unrelated", None).await?;
    let unknown = send(&http, "unknown", "new-unknown", None).await?;
    let overlap = send(&http, "carol", "new-overlap", None).await?;
    assert_raw_owner(&http, &unknown["contextId"], Some("unknown")).await?;
    assert_hidden_context(&http, "alice", &unrelated).await?;
    assert_hidden_context(&http, "unknown", &first).await?;
    assert_hidden_context(&http, "alice", &overlap).await?;
    // Stored Alice isn't expanded to Bob to satisfy Carol's [Bob, Carol] set.
    assert_hidden_context(&http, "carol", &incoming).await?;
    assert_eq!(http.h.metadata.list().await?.len(), 5);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aliases_access_client_paths_with_rules() -> Result<()> {
    alias_client_paths(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aliases_access_client_paths_without_rules() -> Result<()> {
    alias_client_paths(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aliases_access_agent_cards_ignore_display_names() -> Result<()> {
    let http = start(true).await?;
    for (user, status) in [
        ("alice", 200),
        ("bob", 200),
        ("Display", 404),
        ("nobody", 404),
    ] {
        let response = http
            .client
            .get(format!("{}/.well-known/agent-card.json", http.url))
            .header("X-User-ID", user)
            .send()
            .await?;
        assert_eq!(response.status(), status, "{user}");
    }
    assert!(http.h.metadata.list().await?.is_empty());
    assert!(http.h.llm.requests.lock().is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aliases_access_anonymous_mode_keeps_shared_owner() -> Result<()> {
    let h = Harness::start(Script::Text).await?;
    let http = Http::with_identity(
        h,
        None,
        Identity::default().with_user_aliases(Some(aliases())),
    )
    .await?;
    let first = send(&http, "alice", "anonymous-first", None).await?;
    let context = &first["contextId"];
    let followup = send(&http, "bob", "anonymous-followup", Some(context)).await?;
    assert_eq!(followup["contextId"], *context);
    let read = rpc(&http, "carol", "GetTask", json!({"id":first["id"]})).await?;
    assert_eq!(read["result"]["id"], first["id"]);
    let listing = rpc(&http, "unknown", "ListTasks", json!({"contextId":context})).await?;
    assert_eq!(listing["result"]["tasks"].as_array().unwrap().len(), 2);
    assert_raw_owner(&http, context, None).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aliases_access_memberships_never_expand_or_become_owner() -> Result<()> {
    let rules = Arc::new(AccessRules::from_yaml(
        "rules:\n  - agents: [runner@runner]\n    users: [bob, unknown]\n  - agents: [runner@runner]\n    groups: [team]\n  - agents: [runner@runner]\n    roles: [supervisor]\n    scopes: [admin]\n",
    )?);
    let aliases = Arc::new(UserAliases::from_yaml(
        "- name: User\n  identities: [alice, bob]\n- name: Role\n  identities: [operator, supervisor]\n- name: Group\n  identities: [team, supervisors]\n",
    )?);
    let h = Harness::start_with_access_rules(Script::Text, Some(rules.clone())).await?;
    let identity = Identity::with_memberships(
        &["X-User-ID".into()],
        &["X-Groups".into()],
        &["X-Roles".into()],
    )?
    .with_user_aliases(Some(aliases));
    let http = Http::with_identity(h, Some(rules), identity).await?;
    let first = send(&http, "bob", "membership-owner", None).await?;
    for (groups, roles, allowed) in [
        ("alice, team", "", false),
        ("", "operator", false),
        ("", "supervisor", true),
    ] {
        let response: Value = http.client.post(&http.url)
            .header("a2a-version", "1.0")
            .header("X-User-ID", "unknown")
            .header("X-Groups", groups)
            .header("X-Roles", roles)
            .json(&json!({"jsonrpc":"2.0", "id":"memberships", "method":"GetTask", "params":{"id":first["id"]}}))
            .send().await?.json().await?;
        if allowed {
            assert_eq!(response["result"]["id"], first["id"], "{response}");
        } else {
            code(&response, -32001);
        }
    }
    assert_raw_owner(&http, &first["contextId"], Some("bob")).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aliases_access_agent_admission_does_not_union_overlap() -> Result<()> {
    let rules = Arc::new(AccessRules::from_yaml(
        "rules:\n  - agents: [runner@runner]\n    users: [carol]\n",
    )?);
    let h = Harness::start_with_access_rules(Script::Text, Some(rules.clone())).await?;
    let identity = Identity::new(&["X-User-ID".into()])?.with_user_aliases(Some(aliases()));
    let http = Http::with_identity(h, Some(rules), identity).await?;
    for user in ["alice", "bob"] {
        let card = http
            .client
            .get(format!("{}/.well-known/agent-card.json", http.url))
            .header("X-User-ID", user)
            .send()
            .await?;
        assert_eq!(card.status(), 404);
        code(
            &rpc(
                &http,
                user,
                "SendMessage",
                json!({"message":message("not-unioned", None)}),
            )
            .await?,
            -32001,
        );
    }
    assert!(http.h.metadata.list().await?.is_empty());
    assert!(http.h.llm.requests.lock().is_empty());
    let task = send(&http, "carol", "later-entry", None).await?;
    assert_raw_owner(&http, &task["contextId"], Some("carol")).await?;
    Ok(())
}

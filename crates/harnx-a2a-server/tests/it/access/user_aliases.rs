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

async fn configured_memberships(rules: &str, aliases: &str, trust_headers: bool) -> Result<Http> {
    harnx_core::require_nextest();
    let rules = Arc::new(AccessRules::from_yaml(rules)?);
    let aliases = Arc::new(UserAliases::from_yaml(aliases)?);
    let h = Harness::start_with_access_rules(Script::Text, Some(rules.clone())).await?;
    let identity = if trust_headers {
        Identity::with_memberships(
            &["X-User-ID".into()],
            &["X-Groups".into()],
            &["X-Roles".into()],
        )?
    } else {
        Identity::new(&["X-User-ID".into()])?
    }
    .with_user_aliases(Some(aliases));
    Http::with_identity(h, Some(rules), identity).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_group_admits_without_headers_but_role_and_unmatched_caller_do_not() -> Result<()>
{
    let http = configured_memberships(
        "rules:\n  - agents: [runner@runner]\n    groups: [engineering]\n  - agents: [runner@runner]\n    roles: [engineering]\n    scopes: [admin]\n",
        "- name: Group member\n  identities: [alice]\n  groups: [engineering]\n- name: Role member\n  identities: [carol]\n  roles: [engineering]\n",
        false,
    ).await?;
    let task = send(&http, "alice", "configured-group", None).await?;
    assert_raw_owner(&http, &task["contextId"], Some("alice")).await?;
    // The role is active (admin read), but cannot satisfy the prompt group.
    let read = rpc(&http, "carol", "GetTask", json!({"id":task["id"]})).await?;
    assert_eq!(read["result"]["id"], task["id"], "{read}");
    for (user, expected) in [
        ("carol", harnx_a2a_server::handler::PERMISSION_DENIED_CODE),
        ("unknown", -32001),
    ] {
        code(
            &rpc(
                &http,
                user,
                "SendMessage",
                json!({"message":message(user, None)}),
            )
            .await?,
            expected,
        );
    }
    assert_eq!(http.h.llm.requests.lock().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aliases_access_memberships_merge_with_configured_groups_and_roles() -> Result<()> {
    // Config grants prompt, header grants admin. Neither source alone can
    // both create a context and operate another user's context.
    let http = configured_memberships(
        "rules:\n  - agents: [runner@runner]\n    groups: [engineering]\n  - agents: [runner@runner]\n    roles: [reviewer]\n  - agents: [runner@runner]\n    groups: [operators]\n    scopes: [admin]\n",
        "- name: Owner\n  identities: [bob]\n  groups: [engineering]\n- name: Group member\n  identities: [alice]\n  groups: [engineering]\n- name: Role member\n  identities: [carol]\n  roles: [reviewer]\n",
        true,
    ).await?;
    let first = send(&http, "bob", "membership-owner", None).await?;
    for user in ["alice", "carol"] {
        code(
            &rpc(&http, user, "GetTask", json!({"id":first["id"]})).await?,
            -32001,
        );
    }
    for user in ["alice", "carol"] {
        http.h.llm.release.notify_one();
        let response: Value = http.client.post(&http.url)
            .header("a2a-version", "1.0")
            .header("X-User-ID", user)
            .header("X-Groups", "operators")
            .json(&json!({"jsonrpc":"2.0", "id":"merge", "method":"SendMessage", "params":{"message":message(user, None)}}))
            .send().await?.json().await?;
        assert!(response["error"].is_null(), "{response}");
        let task = &response["result"]["task"];
        assert_eq!(
            task["status"]["state"], "TASK_STATE_COMPLETED",
            "{response}"
        );
        assert_raw_owner(&http, &task["contextId"], Some(user)).await?;
        let response: Value = http.client.post(&http.url)
            .header("a2a-version", "1.0")
            .header("X-User-ID", user)
            .header("X-Groups", "operators")
            .json(&json!({"jsonrpc":"2.0", "id":"merged-read", "method":"GetTask", "params":{"id":first["id"]}}))
            .send().await?.json().await?;
        assert_eq!(response["result"]["id"], first["id"], "{response}");
    }
    // Header-only admin can't pass the creation assertion above.
    let response: Value = http.client.post(&http.url)
        .header("a2a-version", "1.0")
        .header("X-User-ID", "unknown")
        .header("X-Groups", "operators")
        .json(&json!({"jsonrpc":"2.0", "id":"header-only", "method":"SendMessage", "params":{"message":message("header-only", None)}}))
        .send().await?.json().await?;
    code(&response, harnx_a2a_server::handler::PERMISSION_DENIED_CODE);
    assert_raw_owner(&http, &first["contextId"], Some("bob")).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aliases_access_memberships_configured_does_not_grant_ownership() -> Result<()> {
    // Membership names equal the owner to catch leakage into user identities.
    let http = configured_memberships(
        "rules:\n  - agents: [runner@runner]\n    groups: [owner]\n  - agents: [runner@runner]\n    roles: [owner]\n",
        "- name: Owner\n  identities: [owner]\n  groups: [owner]\n- name: Group member\n  identities: [alice]\n  groups: [owner]\n- name: Role member\n  identities: [carol]\n  roles: [owner]\n",
        false,
    ).await?;
    let first = send(&http, "owner", "no-ownership-transfer", None).await?;
    for user in ["alice", "carol"] {
        let own = send(&http, user, user, None).await?;
        assert_raw_owner(&http, &own["contextId"], Some(user)).await?;
        code(
            &rpc(&http, user, "GetTask", json!({"id":first["id"]})).await?,
            -32001,
        );
        code(
            &rpc(
                &http,
                user,
                "SendMessage",
                json!({"message":message("not-owner", Some(&first["contextId"]))}),
            )
            .await?,
            -32001,
        );
    }
    assert_raw_owner(&http, &first["contextId"], Some("owner")).await?;
    assert_eq!(http.h.llm.requests.lock().len(), 3);
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

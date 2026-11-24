//! Caller-principals snapshot through real JSON-RPC admission and worker resume.

use crate::{
    support::{Harness, Script},
    unary::{message, Http, HttpRpcCall},
};
use anyhow::{Context, Result};
use harnx_a2a_server::identity::Identity;
use harnx_core::{
    session_identity::session_key,
    user_aliases::UserAliases,
    user_principals::{from_resolved, UserPrincipals},
};
use serde_json::{json, Value};
use std::sync::Arc;

const ALIASES: &str =
    "- name: Team\n  identities: [alice, bob]\n  groups: [engineering]\n  roles: [reviewer]\n";

fn expected_principals() -> UserPrincipals {
    from_resolved(
        Some("alice"),
        Some(&["alice".into(), "bob".into()]),
        &["engineering".into()],
        &["reviewer".into()],
    )
}

async fn start() -> Result<Http> {
    let h = Harness::start(Script::Text).await?;
    let identity = Identity::with_memberships(
        &["X-User-ID".into()],
        &["X-Groups".into()],
        &["X-Roles".into()],
    )?
    .with_user_aliases(Some(Arc::new(UserAliases::from_yaml(ALIASES)?)));
    Http::with_identity(h, None, identity).await
}

async fn send(http: &Http, user: &str, id: &str, context: Option<&Value>) -> Result<Value> {
    http.h.llm.release.notify_one();
    let response = http
        .rpc_call(HttpRpcCall::new(
            &http.url,
            user,
            "SendMessage",
            json!({"message": message(id, context)}),
        ))
        .await?;
    assert!(response["error"].is_null(), "{response}");
    Ok(response["result"]["task"].clone())
}

async fn principals(http: &Http, context: &str) -> Result<UserPrincipals> {
    let key = session_key(Some("runner"), context);
    let record = http
        .h
        .metadata
        .get(&key)
        .await?
        .context("session metadata record")?;
    Ok(harnx_runtime::nats_session_metadata::user_principals(
        &record.metadata,
    ))
}

#[tokio::test]
async fn first_message_stamps_the_callers_resolved_principals() -> Result<()> {
    let http = start().await?;
    let task = send(&http, "alice", "first", None).await?;
    assert_eq!(
        principals(&http, task["contextId"].as_str().context("context")?).await?,
        expected_principals(),
        "exact alias expansion and memberships must survive storage"
    );
    // A later turn from the same caller cannot re-stamp anything.
    let followup = send(&http, "alice", "second", Some(&task["contextId"])).await?;
    assert_eq!(
        principals(&http, followup["contextId"].as_str().context("context")?).await?,
        expected_principals()
    );
    Ok(())
}

#[tokio::test]
async fn retrying_caller_cannot_replace_the_winning_snapshot() -> Result<()> {
    let http = start().await?;
    let first = send(&http, "alice", "retry-first", None).await?;
    let context = first["contextId"].clone();

    // Same messageId again, now claiming a group the first caller never had.
    // The reservation is create-only: the retry's current memberships lose.
    let response = http
        .client
        .post(&http.url)
        .header("a2a-version", "1.0")
        .header("X-User-ID", "alice")
        .header("X-Groups", "intruders")
        .json(&json!({
            "jsonrpc": "2.0", "id": "2", "method": "SendMessage",
            "params": {"message": message("retry-first", None)}
        }))
        .send()
        .await?;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await?;
    assert!(body["error"].is_null(), "{body}");
    assert_eq!(
        body["result"]["task"]["contextId"], context,
        "retry joins the reserved context"
    );
    assert_eq!(
        principals(&http, context.as_str().context("context")?).await?,
        expected_principals(),
        "the winning caller's snapshot stays"
    );
    Ok(())
}

#[tokio::test]
async fn matched_alias_memberships_stamp_for_the_alias_owner() -> Result<()> {
    // Sending as bob resolves through the same alias entry: same snapshot,
    // recorded for the caller that won admission.
    let http = start().await?;
    let task = send(&http, "bob", "as-bob", None).await?;
    let context = task["contextId"].as_str().context("context")?;
    let stored = principals(&http, context).await?;
    assert_eq!(stored.user_id.as_deref(), Some("bob"));
    assert_eq!(stored.users, ["alice", "bob"]);
    assert_eq!(stored.groups, ["engineering"]);
    assert_eq!(stored.roles, ["reviewer"]);
    Ok(())
}

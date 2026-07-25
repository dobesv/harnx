//! End-to-end MCP handler tests over the GitHub backend.
//!
//! These drive the tool handlers the way an MCP client does — plans addressed by
//! name — against the stateful GitHub API mock, which rejects a blank issue title
//! exactly like GitHub. This is the path that returned an opaque 422.

mod github_mock;

use std::sync::Arc;

use harnx_mcp_plans_core::server::{
    AddPlanParams, GetPlanParams, ListPlansParams, PlansServer, UpdatePlanParams,
};
use harnx_mcp_plans_core::PlanStore;
use rmcp::model::{CallToolResult, ContentBlock, ErrorCode};
use serde_json::Value;
use wiremock::MockServer;

use github_mock::{create_test_store_with_server, mount_mock_handlers, MockGitHubState};

async fn plans_server() -> (PlansServer<impl PlanStore>, MockServer) {
    let state = Arc::new(std::sync::Mutex::new(MockGitHubState::new()));
    let server = MockServer::start().await;
    mount_mock_handlers(&server, state).await;
    let store = create_test_store_with_server(&server).await;
    (PlansServer::new(Arc::new(store)), server)
}

async fn plans_server_with_failing_parent(
    parent_issue: u64,
) -> (PlansServer<impl PlanStore>, MockServer) {
    let mut mock_state = MockGitHubState::new();
    mock_state.fail_sub_issue_for_parent(parent_issue, 422);
    let state = Arc::new(std::sync::Mutex::new(mock_state));
    let server = MockServer::start().await;
    mount_mock_handlers(&server, state).await;
    let store = create_test_store_with_server(&server).await;
    (PlansServer::new(Arc::new(store)), server)
}

fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn result_json(result: &CallToolResult) -> Value {
    serde_json::from_str(&result_text(result)).expect("handler result should be JSON")
}

/// Count the plan-list requests the handlers made, i.e. how often the store had to
/// resolve a plan name to an issue number.
async fn plan_list_request_count(mock: &MockServer) -> usize {
    mock.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| {
            request.method == wiremock::http::Method::GET
                && request.url.path() == "/repos/test-owner/test-repo/issues"
        })
        .count()
}

async fn sub_issue_requests(mock: &MockServer) -> Vec<(String, Value)> {
    mock.received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| {
            request.method == wiremock::http::Method::POST
                && request.url.path().ends_with("/sub_issues")
        })
        .map(|request| {
            (
                request.url.path().to_string(),
                serde_json::from_slice(&request.body).expect("sub-issue request should be JSON"),
            )
        })
        .collect()
}

#[tokio::test]
async fn add_plan_with_parent_issue_nests_created_issue_by_internal_id() {
    let (server, mock) = plans_server().await;
    server
        .handle_add_plan(AddPlanParams {
            name: "nested-plan".to_string(),
            parent_issue: Some(42),
            ..Default::default()
        })
        .await
        .expect("add_plan should succeed");

    let requests = sub_issue_requests(&mock).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0],
        (
            "/repos/test-owner/test-repo/issues/42/sub_issues".to_string(),
            serde_json::json!({ "sub_issue_id": 10000 }),
        )
    );
}

#[tokio::test]
async fn add_plan_succeeds_when_parent_sub_issue_link_fails() {
    let (server, _mock) = plans_server_with_failing_parent(42).await;

    server
        .handle_add_plan(AddPlanParams {
            name: "best-effort-nesting".to_string(),
            parent_issue: Some(42),
            content: Some("plan survives failed nesting".to_string()),
            ..Default::default()
        })
        .await
        .expect("plan creation should succeed when sub-issue nesting fails");

    let plan = result_json(
        &server
            .handle_get_plan(GetPlanParams {
                name: "best-effort-nesting".to_string(),
                ..Default::default()
            })
            .await
            .expect("created plan issue should still exist"),
    );
    assert_eq!(plan["body"], "plan survives failed nesting");
}

#[tokio::test]
async fn add_plan_without_parent_issue_does_not_create_sub_issue_link() {
    let (server, mock) = plans_server().await;
    server
        .handle_add_plan(AddPlanParams {
            name: "standalone-plan".to_string(),
            ..Default::default()
        })
        .await
        .expect("add_plan should succeed");

    assert!(sub_issue_requests(&mock).await.is_empty());
}

#[tokio::test]
async fn update_plan_creates_missing_plan_with_its_title() {
    let (server, mock) = plans_server().await;

    server
        .handle_update_plan(UpdatePlanParams {
            name: "decouple-command-usage-1168".to_string(),
            parent_issue: Some(42),
            title: Some("Decouple command usage from the harness".to_string()),
            summary: Some("Pull usage out of the harness".to_string()),
            author: Some("hestia".to_string()),
            git_branch: Some("feature/decouple-usage".to_string()),
            append_content: Some("## Goal\nSplit the usage text.".to_string()),
            ..Default::default()
        })
        .await
        .expect("update_plan should create the plan instead of failing with a 422");

    let plan = result_json(
        &server
            .handle_get_plan(GetPlanParams {
                name: "decouple-command-usage-1168".to_string(),
                ..Default::default()
            })
            .await
            .expect("the created plan should be readable by name"),
    );

    assert_eq!(
        plan["title"], "Decouple command usage from the harness",
        "created issue must carry the requested title"
    );
    assert_eq!(plan["summary"], "Pull usage out of the harness");
    assert_eq!(plan["author"], "hestia");
    assert_eq!(plan["git_branch"], "feature/decouple-usage");
    assert!(
        plan["body"]
            .as_str()
            .expect("body")
            .contains("Split the usage text."),
        "body content should have been written: {plan:?}"
    );
    assert_eq!(
        sub_issue_requests(&mock).await,
        vec![(
            "/repos/test-owner/test-repo/issues/42/sub_issues".to_string(),
            serde_json::json!({ "sub_issue_id": 10000 }),
        )],
        "auto-created plan should honor parent_issue"
    );
}

#[tokio::test]
async fn update_plan_rejects_parent_issue_for_existing_plan() {
    let (server, _mock) = plans_server().await;
    server
        .handle_add_plan(AddPlanParams {
            name: "existing-parent-plan".to_string(),
            ..Default::default()
        })
        .await
        .expect("setup plan should be created");

    let error = server
        .handle_update_plan(UpdatePlanParams {
            name: "existing-parent-plan".to_string(),
            parent_issue: Some(42),
            ..Default::default()
        })
        .await
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
    assert!(error.message.contains(
        "parent_issue can only be set when creating a plan, not when updating an existing plan"
    ));
}

/// Once a plan's name has been resolved, the rest of the call addresses it by the
/// store's canonical ID, so a single `update_plan` costs one name lookup, not one per
/// read and write.
#[tokio::test]
async fn update_plan_resolves_the_plan_name_once() {
    let (server, mock) = plans_server().await;
    server
        .handle_add_plan(AddPlanParams {
            name: "resolve-once".to_string(),
            title: Some("Resolve once".to_string()),
            body: Some("body".to_string()),
            ..Default::default()
        })
        .await
        .expect("add_plan should succeed");

    let before = plan_list_request_count(&mock).await;
    server
        .handle_update_plan(UpdatePlanParams {
            name: "resolve-once".to_string(),
            append_content: Some("more".to_string()),
            ..Default::default()
        })
        .await
        .expect("update_plan should succeed");

    let lookups = plan_list_request_count(&mock).await - before;
    assert_eq!(
        lookups, 1,
        "update_plan should resolve the plan name once, not per read and write"
    );
}

#[tokio::test]
async fn update_plan_appends_to_the_same_plan_instead_of_creating_another() {
    let (server, _mock) = plans_server().await;
    let name = "iterated-plan".to_string();

    for line in ["first", "second", "third"] {
        server
            .handle_update_plan(UpdatePlanParams {
                name: name.clone(),
                title: Some("Iterated plan".to_string()),
                append_content: Some(line.to_string()),
                ..Default::default()
            })
            .await
            .expect("update_plan should succeed");
    }

    let plans = result_json(
        &server
            .handle_list_plans(ListPlansParams::default())
            .await
            .expect("list_plans should succeed"),
    );
    let plans = plans.as_array().expect("array");
    assert_eq!(
        plans.len(),
        1,
        "repeated updates must not create extra plans: {plans:?}"
    );

    let plan = result_json(
        &server
            .handle_get_plan(GetPlanParams {
                name,
                ..Default::default()
            })
            .await
            .expect("get_plan should succeed"),
    );
    assert_eq!(plan["body"], "first\nsecond\nthird");
}

#[tokio::test]
async fn add_plan_then_read_by_name_round_trips() {
    let (server, _mock) = plans_server().await;

    server
        .handle_add_plan(AddPlanParams {
            name: "fresh-plan".to_string(),
            title: Some("Fresh plan".to_string()),
            body: Some("initial body".to_string()),
            ..Default::default()
        })
        .await
        .expect("add_plan should not fail after creating the issue");

    let plan = result_json(
        &server
            .handle_get_plan(GetPlanParams {
                name: "fresh-plan".to_string(),
                ..Default::default()
            })
            .await
            .expect("get_plan should succeed"),
    );
    assert_eq!(plan["title"], "Fresh plan");
    assert_eq!(plan["body"], "initial body");
}

#[tokio::test]
async fn update_plan_without_title_still_creates_a_titled_issue() {
    let (server, _mock) = plans_server().await;

    server
        .handle_update_plan(UpdatePlanParams {
            name: "untitled-plan".to_string(),
            replace_content: Some("notes".to_string()),
            ..Default::default()
        })
        .await
        .expect("update_plan should create the plan without a title param");

    let plan = result_json(
        &server
            .handle_get_plan(GetPlanParams {
                name: "untitled-plan".to_string(),
                ..Default::default()
            })
            .await
            .expect("get_plan should succeed"),
    );
    assert_eq!(
        plan["title"], "untitled-plan",
        "the plan name is the fallback title"
    );
}

#[tokio::test]
async fn content_round_trips_through_github_plan_create_and_update() {
    let (server, _mock) = plans_server().await;
    server
        .handle_add_plan(AddPlanParams {
            name: "content-alias".to_string(),
            title: Some("Content alias".to_string()),
            content: Some("# Initial markdown".to_string()),
            ..Default::default()
        })
        .await
        .expect("add_plan content should be written");

    let created = result_json(
        &server
            .handle_get_plan(GetPlanParams {
                name: "content-alias".to_string(),
                ..Default::default()
            })
            .await
            .unwrap(),
    );
    assert_eq!(created["body"], "# Initial markdown");

    server
        .handle_update_plan(UpdatePlanParams {
            name: "content-alias".to_string(),
            content: Some("# Revised markdown".to_string()),
            ..Default::default()
        })
        .await
        .expect("update_plan content should replace body");
    let updated = result_json(
        &server
            .handle_get_plan(GetPlanParams {
                name: "content-alias".to_string(),
                ..Default::default()
            })
            .await
            .unwrap(),
    );
    assert_eq!(updated["body"], "# Revised markdown");
}

#[tokio::test]
async fn add_plan_rejects_body_with_content() {
    let (server, _mock) = plans_server().await;
    let error = server
        .handle_add_plan(AddPlanParams {
            name: "conflicting-add-content".to_string(),
            body: Some("a".to_string()),
            content: Some("b".to_string()),
            ..Default::default()
        })
        .await
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
    assert!(error
        .message
        .contains("provide at most one of body, content"));
}

#[tokio::test]
async fn update_plan_rejects_content_with_replace_content() {
    let (server, _mock) = plans_server().await;
    let error = server
        .handle_update_plan(UpdatePlanParams {
            name: "conflicting-content".to_string(),
            content: Some("one".to_string()),
            replace_content: Some("two".to_string()),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "provide at most one of content, replace_content, append_content, replace_in_content"
    );
}

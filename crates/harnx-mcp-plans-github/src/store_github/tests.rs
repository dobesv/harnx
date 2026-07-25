//! Integration tests for GitHubPlanStore using wiremock.
//!
//! These tests verify label scoping, cross-plan isolation, dedupe, pagination,
//! delete behavior, and sub-issue cap handling.

use jiff::Timestamp;
use wiremock::{
    matchers::{body_json, body_partial_json, method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

use harnx_mcp_plans_core::{NewPlan, NewTask, PageToken, PlanStore, StoreError, TaskFilter};

use crate::auth::SystemClock;
use crate::auth::{AuthConfig, AuthSource, GitHubAuth};
use crate::client::{GitHubClient, GitHubClientFactory};
use crate::ratelimit::{RateLimitConfig, RateLimitExecutor, TokioSleeper};
use std::sync::Arc;

use super::*;

pub(super) async fn create_test_store(server: &MockServer) -> GitHubPlanStore {
    let config = AuthConfig {
        base_url: server.uri(),
        source: AuthSource::PersonalAccessToken("test-token".to_string()),
    };

    let auth = GitHubAuth::new(config).unwrap();
    let factory = GitHubClientFactory::new(
        auth,
        Arc::new(RateLimitExecutor::new(
            Arc::new(SystemClock),
            Arc::new(TokioSleeper),
            RateLimitConfig::default(),
        )),
    )
    .unwrap();

    GitHubPlanStore::new(factory)
}

async fn create_test_store_with_config(
    server: &MockServer,
    config: GitHubStoreConfig,
) -> GitHubPlanStore {
    let auth_config = AuthConfig {
        base_url: server.uri(),
        source: AuthSource::PersonalAccessToken("test-token".to_string()),
    };

    let auth = GitHubAuth::new(auth_config).unwrap();
    let factory = GitHubClientFactory::new(
        auth,
        Arc::new(RateLimitExecutor::new(
            Arc::new(SystemClock),
            Arc::new(TokioSleeper),
            RateLimitConfig::default(),
        )),
    )
    .unwrap();

    GitHubPlanStore::with_config(factory, config)
}

pub(super) fn target(owner: &str, repo: &str) -> harnx_mcp_plans_core::Target {
    harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

async fn create_store_with_default(
    server: &MockServer,
    owner: &str,
    repo: &str,
) -> GitHubPlanStore {
    let config = AuthConfig {
        base_url: server.uri(),
        source: AuthSource::PersonalAccessToken("test-token".to_string()),
    };

    let auth = GitHubAuth::new(config).unwrap();
    let client = GitHubClient::new(auth, owner, repo).await.unwrap();
    GitHubPlanStore::new(client)
}
fn mock_issue_json(id: u64, number: u64, title: &str, body: &str) -> serde_json::Value {
    let created = Timestamp::now().to_string();
    serde_json::json!({
        "id": id,
        "number": number,
        "title": title,
        "body": body,
        "node_id": format!("I_kwDOA{}", id),
        "state": "open",
        "labels": [],
        "created_at": created,
        "updated_at": created,
        "comments": 0
    })
}

fn mock_issue_with_labels_json(
    id: u64,
    number: u64,
    title: &str,
    body: &str,
    labels: &[&str],
) -> serde_json::Value {
    let created = Timestamp::now().to_string();
    serde_json::json!({
        "id": id,

        "number": number,
        "title": title,
        "body": body,
        "node_id": format!("I_kwDOA{}", id),
        "state": "open",
        "labels": labels.iter().map(|name| serde_json::json!({"id": 1, "name": name})).collect::<Vec<_>>(),
        "created_at": created,
        "updated_at": created,
        "comments": 0
    })
}

fn mock_comment_json(id: u64, body: &str) -> serde_json::Value {
    let created = Timestamp::now().to_string();
    serde_json::json!({
        "id": id,
        "body": body,
        "created_at": created,
        "updated_at": created
    })
}

#[tokio::test]
async fn client_for_rejects_path_traversal_without_request() {
    let server = MockServer::start().await;
    let store = create_test_store(&server).await;
    let traversal = RepoTarget {
        owner: "acme".to_string(),
        repo: "../../../user".to_string(),
    };

    let err = store
        .client_for(&traversal)
        .expect_err("repo traversal should be rejected");
    assert!(matches!(err, StoreError::InvalidParams(_)));

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        requests.is_empty(),
        "invalid target made GitHub API requests: {requests:?}"
    );
}
#[tokio::test]
async fn explicit_target_rejects_path_traversal_without_request() {
    let server = MockServer::start().await;
    let store = create_test_store(&server).await;

    let repo_err = store
        .list_plans(&target("acme", "../../../user"), None)
        .await
        .expect_err("repo traversal should be rejected");
    assert!(matches!(repo_err, StoreError::InvalidParams(_)));

    let owner_err = store
        .list_plans(&target("../../../user", "plans"), None)
        .await
        .expect_err("owner traversal should be rejected");
    assert!(matches!(owner_err, StoreError::InvalidParams(_)));

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        requests.is_empty(),
        "invalid targets made GitHub API requests: {requests:?}"
    );
}

#[tokio::test]
async fn explicit_target_routes_requests_to_requested_repo() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/target-owner/target-repo/issues"))
        .and(query_param("labels", "harnx-plan"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .expect(1)
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let page = store
        .list_plans(&target("target-owner", "target-repo"), None)
        .await
        .unwrap();
    assert!(page.items.is_empty());
}

#[tokio::test]
async fn explicit_target_accepts_repo_slug_with_dot() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/plans.rs/issues"))
        .and(query_param("labels", "harnx-plan"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .expect(1)
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let page = store
        .list_plans(&target("acme", "plans.rs"), None)
        .await
        .unwrap();
    assert!(page.items.is_empty());
}

#[tokio::test]
async fn local_target_falls_back_to_default_repo_for_requests() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/default-owner/default-repo/issues"))
        .and(query_param("labels", "harnx-plan"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .expect(1)
        .mount(&server)
        .await;

    let store = create_store_with_default(&server, "default-owner", "default-repo").await;
    let page = store
        .list_plans(&harnx_mcp_plans_core::Target::Local, None)
        .await
        .unwrap();
    assert!(page.items.is_empty());
}

#[tokio::test]
async fn constructing_store_does_not_call_github_api() {
    let server = MockServer::start().await;
    let _store = create_test_store(&server).await;
    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        requests.is_empty(),
        "store construction made GitHub API requests: {requests:?}"
    );
}

#[tokio::test]
async fn ensure_label_failure_still_creates_plan() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/labels/harnx-plan"))
        .respond_with(ResponseTemplate::new(400).set_body_string("boom"))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/test-owner/test-repo/issues"))
        .and(body_partial_json(
            serde_json::json!({ "labels": ["harnx-plan"] }),
        ))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(mock_issue_with_labels_json(
                1002,
                2,
                "Plan B",
                "---\nclient_id: plan-b\n---\n",
                &["harnx-plan"],
            )),
        )
        .expect(1)
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let plan = store
        .add_plan(
            &target("test-owner", "test-repo"),
            NewPlan {
                id: "plan-b".to_string(),
                title: Some("Plan B".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(plan.id, "2");
}

#[tokio::test]
async fn create_label_conflict_is_handled_when_ensuring_plan_label() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/labels/harnx-plan"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/test-owner/test-repo/labels"))
        .respond_with(ResponseTemplate::new(422).set_body_string("already_exists"))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/test-owner/test-repo/issues"))
        .and(body_partial_json(
            serde_json::json!({ "labels": ["harnx-plan"] }),
        ))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(mock_issue_with_labels_json(
                1003,
                3,
                "Plan C",
                "---\nclient_id: plan-c\n---\n",
                &["harnx-plan"],
            )),
        )
        .expect(1)
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let plan = store
        .add_plan(
            &target("test-owner", "test-repo"),
            NewPlan {
                id: "plan-c".to_string(),
                title: Some("Plan C".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(plan.id, "3");
}

#[tokio::test]
async fn create_with_label_validation_failure_retries_without_label() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/labels/harnx-plan"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1,
            "name": "harnx-plan"
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/test-owner/test-repo/issues"))
        .and(body_partial_json(
            serde_json::json!({ "labels": ["harnx-plan"] }),
        ))
        .respond_with(
            ResponseTemplate::new(422).set_body_string("Validation Failed: invalid label"),
        )
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/test-owner/test-repo/issues"))
        .and(body_partial_json(serde_json::json!({
            "title": "Plan D"
        })))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(mock_issue_with_labels_json(
                1004,
                4,
                "Plan D",
                "---\nclient_id: plan-d\n---\n",
                &[],
            )),
        )
        .expect(1)
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let plan = store
        .add_plan(
            &target("test-owner", "test-repo"),
            NewPlan {
                id: "plan-d".to_string(),
                title: Some("Plan D".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(plan.id, "4");
}

#[tokio::test]
async fn add_plan_applies_plan_label() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/repos/test-owner/test-repo/issues"))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(mock_issue_with_labels_json(
                1001,
                1,
                "Plan A",
                "---\nclient_id: plan-a\n---\n",
                &["harnx-plan"],
            )),
        )
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let plan = store
        .add_plan(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }),
            NewPlan {
                id: "plan-a".to_string(),
                title: Some("Plan A".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(plan.id, "1");
}

#[tokio::test]
async fn get_plan_unlabeled_issue_returns_not_found() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/7"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_issue_with_labels_json(
                7001,
                7,
                "Repo Issue",
                "---\n---\n",
                &["other"],
            )),
        )
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let err = store
        .get_plan(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }),
            &"7".to_string(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound));
}

#[tokio::test]
async fn delete_plan_unlabeled_issue_returns_not_found() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/7"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_issue_with_labels_json(
                7001,
                7,
                "Repo Issue",
                "---\n---\n",
                &["other"],
            )),
        )
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let err = store
        .delete_plan(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }),
            &"7".to_string(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound));
}

#[tokio::test]
async fn list_plans_filters_non_plan_issues_and_dedupes() {
    let server = MockServer::start().await;
    let now = Timestamp::now();
    let earlier = now - std::time::Duration::from_secs(60);

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues"))
        .and(query_param("state", "open"))
        .and(query_param("labels", "harnx-plan"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![
            serde_json::json!({
                "id": 2001,
                "number": 11,
                "title": "Plan Older",
                "body": "---\nclient_id: shared-plan\n---\n",
                "labels": [{"id": 1, "name": "harnx-plan"}],
                "state": "open",
                "created_at": now.to_string(),
                "updated_at": earlier.to_string(),
                "comments": 0
            }),
            serde_json::json!({
                "id": 2002,
                "number": 12,
                "title": "Plan Newer",
                "body": "---\nclient_id: shared-plan\n---\n",
                "labels": [{"id": 1, "name": "harnx-plan"}],
                "state": "open",
                "created_at": now.to_string(),
                "updated_at": now.to_string(),
                "comments": 0
            }),
            mock_issue_with_labels_json(2003, 99, "Not A Plan", "---\n---\n", &["other"]),
        ]))
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let page = store
        .list_plans(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }),
            None,
        )
        .await
        .unwrap();

    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, "12");
}

#[tokio::test]
async fn list_notes_dedupes_by_client_id_keeps_most_recent() {
    let server = MockServer::start().await;
    let now = Timestamp::now();
    let earlier = now - std::time::Duration::from_secs(60);

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![
            serde_json::json!({
                "id": 5001,
                "body": "---\nclient_id: shared-note\n---\n",
                "created_at": now.to_string(),
                "updated_at": earlier.to_string()
            }),
            serde_json::json!({
                "id": 5002,
                "body": "---\nclient_id: shared-note\n---\n",
                "created_at": now.to_string(),
                "updated_at": now.to_string()
            }),
        ]))
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let page = store
        .list_notes(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }),
            &"1".to_string(),
            None,
        )
        .await
        .unwrap();

    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, "5002");
}

#[tokio::test]
async fn dedupe_tie_break_prefers_higher_issue_number() {
    let server = MockServer::start().await;
    let now = Timestamp::now();

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues"))
        .and(query_param("state", "open"))
        .and(query_param("labels", "harnx-plan"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![
            serde_json::json!({
                "id": 3001,
                "number": 21,
                "title": "Plan 21",
                "body": "---\nclient_id: same-plan\n---\n",
                "labels": [{"id": 1, "name": "harnx-plan"}],
                "state": "open",
                "created_at": now.to_string(),
                "updated_at": now.to_string(),
                "comments": 0
            }),
            serde_json::json!({
                "id": 3002,
                "number": 22,
                "title": "Plan 22",
                "body": "---\nclient_id: same-plan\n---\n",
                "labels": [{"id": 1, "name": "harnx-plan"}],
                "state": "open",
                "created_at": now.to_string(),
                "updated_at": now.to_string(),
                "comments": 0
            }),
        ]))
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let page = store
        .list_plans(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, "22");
}

#[tokio::test]
async fn add_task_sends_correct_internal_id_in_post_body() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_issue_with_labels_json(
                9001,
                1,
                "Plan",
                "---\nclient_id: plan-1\n---\n",
                &["harnx-plan"],
            )),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/1/sub_issues"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&server)
        .await;

    let task_internal_id = 12345678u64;
    let task_number = 42u64;

    Mock::given(method("POST"))
        .and(path("/repos/test-owner/test-repo/issues"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mock_issue_json(
            task_internal_id,
            task_number,
            "Test Task",
            "---\nclient_id: task-1\n---\n",
        )))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/test-owner/test-repo/issues/1/sub_issues"))
        .and(body_json(
            serde_json::json!({ "sub_issue_id": task_internal_id }),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_string("{}"))
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;

    let result = store
        .add_task(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }),
            &"1".to_string(),
            NewTask {
                id: "task-1".to_string(),
                title: "Test Task".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                dependencies: vec![],
            },
        )
        .await;
    assert!(result.is_ok(), "add_task failed: {:?}", result.err());
    assert_eq!(result.unwrap().id, "42");
}

#[tokio::test]
async fn add_task_returns_error_when_cap_reached_without_creating_issue() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_issue_with_labels_json(
                9001,
                1,
                "Plan",
                "---\nclient_id: plan-1\n---\n",
                &["harnx-plan"],
            )),
        )
        .mount(&server)
        .await;

    let next_url = format!("{}/sub-page-2", server.uri());
    let first_page: Vec<_> = (0..60)
        .map(|i| serde_json::json!({"number": i + 1, "title": format!("Task {}", i)}))
        .collect();
    let second_page: Vec<_> = (60..100)
        .map(|i| serde_json::json!({"number": i + 1, "title": format!("Task {}", i)}))
        .collect();

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/1/sub_issues"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&first_page)
                .insert_header("Link", format!("<{next_url}>; rel=\"next\"")),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/sub-page-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&second_page))
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;

    let result = store
        .add_task(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }),
            &"1".to_string(),
            NewTask {
                id: "task-overflow".to_string(),
                title: "Overflow Task".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                dependencies: vec![],
            },
        )
        .await;
    assert!(result.is_err());
    match result.unwrap_err() {
        StoreError::InvalidParams(msg) => {
            assert!(msg.contains("maximum number of sub-issues"));
        }
        other => panic!("expected InvalidParams error, got {other:?}"),
    }
}

#[tokio::test]
async fn get_task_wrong_plan_returns_not_found() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_issue_with_labels_json(
                9002,
                2,
                "Plan 2",
                "---\nclient_id: plan-2\n---\n",
                &["harnx-plan"],
            )),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/2/sub_issues"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(vec![serde_json::json!({
                "number": 999,
                "title": "Other task",
                "body": "---\n---\n",
                "created_at": Timestamp::now().to_string(),
                "updated_at": Timestamp::now().to_string()
            })]),
        )
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let err = store
        .get_task(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }),
            &"2".to_string(),
            &"42".to_string(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound));
}

#[tokio::test]
async fn get_task_on_second_page_succeeds() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_issue_with_labels_json(
                9001,
                1,
                "Plan",
                "---\nclient_id: plan-1\n---\n",
                &["harnx-plan"],
            )),
        )
        .mount(&server)
        .await;

    let next_url = format!("{}/sub-page-2", server.uri());
    let first_page: Vec<_> = (0..30)
        .map(|i| {
            serde_json::json!({
                "id": 10000 + i,
                "number": i + 1,
                "title": format!("Task {}", i),
                "body": "---\nclient_id: t\n---\n",
                "state": "open",
                "created_at": Timestamp::now().to_string(),
                "updated_at": Timestamp::now().to_string()
            })
        })
        .collect();
    let second_page = vec![serde_json::json!({
        "id": 20000,
        "number": 31,
        "title": "Task 31",
        "body": "---\nclient_id: page-two-task\n---\n",
        "labels": [],
        "state": "open",
        "created_at": Timestamp::now().to_string(),
        "updated_at": Timestamp::now().to_string(),
        "comments": 0
    })];

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/1/sub_issues"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&first_page)
                .insert_header("Link", format!("<{next_url}>; rel=\"next\"")),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/sub-page-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&second_page))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/31"))
        .respond_with(ResponseTemplate::new(200).set_body_json(second_page[0].clone()))
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let task = store
        .get_task(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }),
            &"1".to_string(),
            &"31".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(task.id, "31");
}

#[tokio::test]
async fn delete_task_wrong_plan_returns_not_found() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_issue_with_labels_json(
                9002,
                2,
                "Plan 2",
                "---\nclient_id: plan-2\n---\n",
                &["harnx-plan"],
            )),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/2/sub_issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let err = store
        .delete_task(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }),
            &"2".to_string(),
            &"42".to_string(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound));
}

#[tokio::test]
async fn get_note_wrong_plan_returns_not_found() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_issue_with_labels_json(
                9002,
                2,
                "Plan 2",
                "---\nclient_id: plan-2\n---\n",
                &["harnx-plan"],
            )),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/comments/5001"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(mock_comment_json(5001, "---\nclient_id: note-a\n---\n")),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/2/comments"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(vec![mock_comment_json(
                5002,
                "---\nclient_id: note-b\n---\n",
            )]),
        )
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let err = store
        .get_note(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }),
            &"2".to_string(),
            &"5001".to_string(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound));
}

#[tokio::test]
async fn get_note_on_second_page_succeeds() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_issue_with_labels_json(
                9001,
                1,
                "Plan",
                "---\nclient_id: plan-1\n---\n",
                &["harnx-plan"],
            )),
        )
        .mount(&server)
        .await;

    let next_url = format!("{}/comments-page-2", server.uri());
    let first_page: Vec<_> = (0..100)
        .map(|i| {
            serde_json::json!({
                "id": 6000 + i,
                "body": "---\nclient_id: note\n---\n",
                "created_at": Timestamp::now().to_string(),
                "updated_at": Timestamp::now().to_string()
            })
        })
        .collect();
    let second_page = vec![serde_json::json!({
        "id": 7001,
        "body": "---\nclient_id: page-two-note\n---\n",
        "created_at": Timestamp::now().to_string(),
        "updated_at": Timestamp::now().to_string()
    })];

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/comments/7001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(second_page[0].clone()))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/1/comments"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&first_page)
                .insert_header("Link", format!("<{next_url}>; rel=\"next\"")),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/comments-page-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&second_page))
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let note = store
        .get_note(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }),
            &"1".to_string(),
            &"7001".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(note.id, "7001");
}

#[tokio::test]
async fn delete_note_wrong_plan_returns_not_found() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_issue_with_labels_json(
                9002,
                2,
                "Plan 2",
                "---\nclient_id: plan-2\n---\n",
                &["harnx-plan"],
            )),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/comments/5001"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(mock_comment_json(5001, "---\nclient_id: note-a\n---\n")),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/2/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let err = store
        .delete_note(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }),
            &"2".to_string(),
            &"5001".to_string(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound));
}

#[tokio::test]
async fn delete_plan_closes_issue() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/123"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_issue_with_labels_json(
                12345,
                123,
                "Test Plan",
                "---\n---\n",
                &["harnx-plan"],
            )),
        )
        .mount(&server)
        .await;

    Mock::given(method("PATCH"))
        .and(path("/repos/test-owner/test-repo/issues/123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_issue_json(
            12345,
            123,
            "Test Plan",
            "---\n---\n",
        )))
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    assert!(store
        .delete_plan(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string()
            }),
            &"123".to_string()
        )
        .await
        .is_ok());
}

#[tokio::test]
async fn delete_task_closes_issue() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_issue_with_labels_json(
                9001,
                1,
                "Plan",
                "---\nclient_id: plan-1\n---\n",
                &["harnx-plan"],
            )),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/1/sub_issues"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(vec![serde_json::json!({
                "id": 67890,
                "number": 456,
                "title": "Test Task",
                "body": "---\n---\n",
                "state": "open",
                "created_at": Timestamp::now().to_string(),
                "updated_at": Timestamp::now().to_string()
            })]),
        )
        .mount(&server)
        .await;

    Mock::given(method("PATCH"))
        .and(path("/repos/test-owner/test-repo/issues/456"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_issue_json(
            67890,
            456,
            "Test Task",
            "---\n---\n",
        )))
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    assert!(store
        .delete_task(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string()
            }),
            &"1".to_string(),
            &"456".to_string()
        )
        .await
        .is_ok());
}

#[tokio::test]
async fn delete_leave_mode_is_honest() {
    let server = MockServer::start().await;
    let store = create_test_store_with_config(
        &server,
        GitHubStoreConfig {
            plan_label: "harnx-plan".to_string(),
            delete_is_close: false,
        },
    )
    .await;

    let err = store
        .delete_plan(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }),
            &"123".to_string(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidParams(_)));
}

#[tokio::test]
async fn delete_note_deletes_comment() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_issue_with_labels_json(
                9001,
                1,
                "Plan",
                "---\nclient_id: plan-1\n---\n",
                &["harnx-plan"],
            )),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/comments/789"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_comment_json(789, "---\n---\n")),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/1/comments"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(vec![mock_comment_json(789, "---\n---\n")]),
        )
        .mount(&server)
        .await;

    Mock::given(method("DELETE"))
        .and(path("/repos/test-owner/test-repo/issues/comments/789"))
        .respond_with(ResponseTemplate::new(204).set_body_string(""))
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    assert!(store
        .delete_note(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string()
            }),
            &"1".to_string(),
            &"789".to_string()
        )
        .await
        .is_ok());
}

#[tokio::test]
async fn list_tasks_dedupes_by_client_id_keeps_most_recent() {
    let server = MockServer::start().await;

    let now = Timestamp::now();
    let earlier = now - std::time::Duration::from_secs(60);

    let task1 = serde_json::json!({
        "number": 101,
        "title": "Task 101",
        "body": "---\nclient_id: shared-id\n---\n",
        "created_at": now.to_string(),
        "updated_at": earlier.to_string()
    });
    let task2 = serde_json::json!({
        "number": 102,
        "title": "Task 102",
        "body": "---\nclient_id: shared-id\n---\n",
        "created_at": now.to_string(),
        "updated_at": now.to_string()
    });

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_issue_with_labels_json(
                9001,
                1,
                "Plan",
                "---\nclient_id: plan-1\n---\n",
                &["harnx-plan"],
            )),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/1/sub_issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![task1, task2]))
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let page = store
        .list_tasks(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }),
            &"1".to_string(),
            TaskFilter::default(),
            None,
        )
        .await
        .unwrap();

    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, "102");
}

#[tokio::test]
async fn write_task_body_does_not_corrupt_front_matter() {
    let decoded = crate::codec::issue_to_task(
        "1".to_string(),
        42,
        "Task",
        Some("---\nclient_id: task-1\nsummary: frontmatter value\n---\nold body"),
        Timestamp::now(),
        None,
    );
    let body = crate::codec::task_meta_update_to_issue_body(
        &decoded,
        &harnx_mcp_plans_core::TaskMetaUpdate::default(),
    )
    .1;
    assert!(body.contains("summary: frontmatter value"));
    assert!(body.ends_with("old body"));
}

#[tokio::test]
async fn list_plans_encodes_link_header_in_page_token() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(vec![mock_issue_with_labels_json(
                    111,
                    1,
                    "Plan 1",
                    "---\n---\n",
                    &["harnx-plan"],
                )])
                .insert_header("Link", r#"<http://example.com/page2>; rel="next""#),
        )
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let page = store
        .list_plans(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }),
            None,
        )
        .await
        .unwrap();

    assert!(page.next.is_some());
    let token = page.next.unwrap();
    assert!(token.0.contains("page2"));
}

#[tokio::test]
async fn list_plans_uses_token_for_next_page() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/page2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(vec![mock_issue_with_labels_json(
                222,
                2,
                "Plan 2",
                "---\n---\n",
                &["harnx-plan"],
            )]),
        )
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let page = store
        .list_plans(
            &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }),
            Some(PageToken(format!("{}/page2", server.uri()))),
        )
        .await
        .unwrap();

    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, "2");
}

#[tokio::test]
async fn retention_pass_closes_stale_plan() {
    let server = MockServer::start().await;
    let stale = (Timestamp::now() - std::time::Duration::from_secs(60 * 60 * 24 * 30)).to_string();

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(vec![serde_json::json!({
                "id": 1234,
                "number": 1,
                "title": "Old Plan",
                "body": "---\n---\n",
                "labels": [{"id": 1, "name": "harnx-plan"}],
                "state": "open",
                "created_at": stale,
                "updated_at": stale,
                "comments": 0
            })]),
        )
        .mount(&server)
        .await;

    Mock::given(method("PATCH"))
        .and(path("/repos/test-owner/test-repo/issues/1"))
        .and(body_json(serde_json::json!({ "state": "closed" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_issue_json(
            1234,
            1,
            "Old Plan",
            "---\n---\n",
        )))
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    crate::runtime::run_retention_pass(
        &store,
        &harnx_mcp_plans_core::Target::GitHub(harnx_mcp_plans_core::RepoTarget {
            owner: "test-owner".to_string(),
            repo: "test-repo".to_string(),
        }),
        14,
    )
    .await
    .unwrap();
}

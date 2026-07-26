//! Tests for addressing plans by name (the `client_id` front-matter) rather than
//! by issue number, plus the label-vs-other classification of a 422 on create.

use jiff::Timestamp;
use wiremock::{
    matchers::{body_partial_json, method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

use harnx_mcp_plans_core::{NewPlan, PlanStore, StoreError};

use super::tests::{create_test_store, target};

/// A plan issue as GitHub would return it, including the `client_id` front-matter
/// that records the user-facing plan name.
struct PlanIssue<'a> {
    number: u64,
    title: &'a str,
    client_id: &'a str,
    body: &'a str,
    updated_at: &'a str,
}

fn plan_issue_json(issue: PlanIssue<'_>) -> serde_json::Value {
    let PlanIssue {
        number,
        title,
        client_id,
        body,
        updated_at,
    } = issue;
    let created = Timestamp::now().to_string();
    let id = 1000 + number;
    serde_json::json!({
        "id": id,
        "number": number,
        "title": title,
        "body": format!("---\nclient_id: {client_id}\ncreated_at: {created}\n---\n{body}"),
        "node_id": format!("I_kwDOA{}", id),
        "state": "open",
        "labels": [{"id": 1, "name": "harnx-plan"}],
        "created_at": created,
        "updated_at": updated_at,
        "comments": 0
    })
}

async fn mount_plan_list(server: &MockServer, issues: Vec<serde_json::Value>) {
    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues"))
        .and(query_param("labels", "harnx-plan"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issues))
        .mount(server)
        .await;
}

#[tokio::test]
async fn get_plan_resolves_plan_name_to_issue_number() {
    let server = MockServer::start().await;
    let updated = Timestamp::now().to_string();
    let issue = plan_issue_json(PlanIssue {
        number: 7,
        title: "Decouple usage",
        client_id: "decouple-command-usage",
        body: "Plan body",
        updated_at: &updated,
    });

    mount_plan_list(&server, vec![issue.clone()]).await;
    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue))
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let plan = store
        .get_plan(
            &target("test-owner", "test-repo"),
            &"decouple-command-usage".to_string(),
        )
        .await
        .expect("plan addressed by name should resolve");

    assert_eq!(plan.id, "7");
    assert_eq!(plan.title, Some("Decouple usage".to_string()));
}

#[tokio::test]
async fn read_and_write_plan_body_resolve_plan_name() {
    let server = MockServer::start().await;
    let updated = Timestamp::now().to_string();
    let issue = plan_issue_json(PlanIssue {
        number: 7,
        title: "Decouple usage",
        client_id: "decouple-command-usage",
        body: "Original body",
        updated_at: &updated,
    });

    mount_plan_list(&server, vec![issue.clone()]).await;
    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue.clone()))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/repos/test-owner/test-repo/issues/7"))
        .and(body_partial_json(serde_json::json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue))
        .expect(1)
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let plan_name = "decouple-command-usage".to_string();
    let target = target("test-owner", "test-repo");

    let body = store
        .read_plan_body(&target, &plan_name)
        .await
        .expect("body addressed by name should resolve");
    assert_eq!(body, "Original body");

    store
        .write_plan_body(&target, &plan_name, "New body")
        .await
        .expect("write addressed by name should resolve");
}

#[tokio::test]
async fn update_plan_meta_resolves_plan_name() {
    let server = MockServer::start().await;
    let updated = Timestamp::now().to_string();
    let issue = plan_issue_json(PlanIssue {
        number: 7,
        title: "Old title",
        client_id: "decouple-command-usage",
        body: "Body",
        updated_at: &updated,
    });

    mount_plan_list(&server, vec![issue.clone()]).await;
    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/repos/test-owner/test-repo/issues/7"))
        .and(body_partial_json(
            serde_json::json!({ "title": "New title" }),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(plan_issue_json(PlanIssue {
                number: 7,
                title: "New title",
                client_id: "decouple-command-usage",
                body: "Body",
                updated_at: &Timestamp::now().to_string(),
            })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let plan = store
        .update_plan_meta(
            &target("test-owner", "test-repo"),
            &"decouple-command-usage".to_string(),
            harnx_mcp_plans_core::PlanMetaUpdate {
                title: Some("New title".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("meta update addressed by name should resolve");

    assert_eq!(plan.title, Some("New title".to_string()));
}

#[tokio::test]
async fn unknown_plan_name_is_not_found() {
    let server = MockServer::start().await;
    mount_plan_list(&server, vec![]).await;

    let store = create_test_store(&server).await;
    let err = store
        .get_plan(&target("test-owner", "test-repo"), &"nope".to_string())
        .await
        .expect_err("unknown plan name");

    assert!(matches!(err, StoreError::NotFound), "got {err:?}");
}

#[tokio::test]
async fn duplicate_plan_names_resolve_to_most_recently_updated_issue() {
    let server = MockServer::start().await;
    let older = (Timestamp::now() - std::time::Duration::from_secs(3600)).to_string();
    let newer = Timestamp::now().to_string();
    let winner = plan_issue_json(PlanIssue {
        number: 5,
        title: "Dup",
        client_id: "dup-plan",
        body: "new",
        updated_at: &newer,
    });

    mount_plan_list(
        &server,
        vec![
            plan_issue_json(PlanIssue {
                number: 3,
                title: "Dup",
                client_id: "dup-plan",
                body: "old",
                updated_at: &older,
            }),
            winner.clone(),
        ],
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/issues/5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(winner))
        .expect(1)
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let plan = store
        .get_plan(&target("test-owner", "test-repo"), &"dup-plan".to_string())
        .await
        .expect("duplicate names resolve deterministically");

    assert_eq!(plan.id, "5");
}

#[tokio::test]
async fn non_label_validation_failure_is_not_retried_without_label() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/test-owner/test-repo/labels/harnx-plan"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1,
            "name": "harnx-plan"
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/test-owner/test-repo/issues"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "message": "Validation Failed",
            "errors": [{"resource": "Issue", "field": "title", "code": "invalid",
                        "message": "title can't be blank"}],
        })))
        .expect(1)
        .mount(&server)
        .await;

    let store = create_test_store(&server).await;
    let err = store
        .add_plan(
            &target("test-owner", "test-repo"),
            NewPlan {
                id: "plan-e".to_string(),
                title: Some("Plan E".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect_err("validation failure");

    let message = format!("{err:?}");
    assert!(
        message.contains("title can't be blank"),
        "error should surface the GitHub validation detail: {message}"
    );
}

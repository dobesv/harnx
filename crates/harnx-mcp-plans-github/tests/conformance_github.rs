//! Conformance test suite for GitHubPlanStore against shared conformance suite.
//!
//! This test uses stateful Wiremock GitHub API mock and runs universal
//! `run_conformance` harness with GitHub capability flags.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use jiff::Timestamp;
use wiremock::{matchers::method, Mock, MockServer, Request, ResponseTemplate};

use harnx_mcp_plans_core::conformance::{run_conformance, BackendCapabilities};
use harnx_mcp_plans_github::auth::{AuthConfig, AuthSource, GitHubAuth};
use harnx_mcp_plans_github::client::GitHubClient;
use harnx_mcp_plans_github::store_github::GitHubPlanStore;

#[derive(Debug)]
struct MockGitHubState {
    issues: HashMap<u64, MockIssue>,
    comments: HashMap<u64, MockComment>,
    sub_issues: HashMap<u64, Vec<u64>>,
    next_issue_id: u64,
    next_comment_id: u64,
}

#[derive(Debug, Clone)]
struct MockIssue {
    id: u64,
    number: u64,
    title: String,
    body: Option<String>,
    labels: Vec<String>,
    state: String,
    created_at: Timestamp,
    updated_at: Option<Timestamp>,
    comments_count: u64,
}

#[derive(Debug, Clone)]
struct MockComment {
    id: u64,
    issue_number: u64,
    body: Option<String>,
    created_at: Timestamp,
    updated_at: Option<Timestamp>,
}

impl MockGitHubState {
    fn new() -> Self {
        Self {
            issues: HashMap::new(),
            comments: HashMap::new(),
            sub_issues: HashMap::new(),
            next_issue_id: 10000,
            next_comment_id: 50000,
        }
    }

    fn create_issue(
        &mut self,
        title: String,
        body: Option<String>,
        labels: Vec<String>,
    ) -> MockIssue {
        let id = self.next_issue_id;
        self.next_issue_id += 1;
        let number = self.next_issue_id - 10000;

        let created_at = Timestamp::now();
        let issue = MockIssue {
            id,
            number,
            title,
            body,
            labels,
            state: "open".to_string(),
            created_at,
            updated_at: None,
            comments_count: 0,
        };
        self.issues.insert(number, issue);
        self.issues.get(&number).unwrap().clone()
    }

    fn get_issue(&self, number: u64) -> Option<&MockIssue> {
        self.issues.get(&number)
    }

    fn update_issue(
        &mut self,
        number: u64,
        title: Option<String>,
        body: Option<String>,
        state: Option<String>,
    ) -> Option<MockIssue> {
        let issue = self.issues.get_mut(&number)?;
        if let Some(t) = title {
            issue.title = t;
        }
        if let Some(b) = body {
            issue.body = Some(b);
        }
        if let Some(s) = state {
            issue.state = s;
        }
        issue.updated_at = Some(Timestamp::now());
        Some(issue.clone())
    }

    fn create_comment(&mut self, issue_number: u64, body: String) -> MockComment {
        let id = self.next_comment_id;
        self.next_comment_id += 1;
        let created_at = Timestamp::now();
        let comment = MockComment {
            id,
            issue_number,
            body: Some(body),
            created_at,
            updated_at: None,
        };

        if let Some(issue) = self.issues.get_mut(&issue_number) {
            issue.comments_count += 1;
        }

        self.comments.insert(id, comment.clone());
        self.comments.get(&id).unwrap().clone()
    }

    fn get_comment(&self, comment_id: u64) -> Option<&MockComment> {
        self.comments.get(&comment_id)
    }

    fn update_comment(&mut self, comment_id: u64, body: String) -> Option<MockComment> {
        let comment = self.comments.get_mut(&comment_id)?;
        comment.body = Some(body);
        comment.updated_at = Some(Timestamp::now());
        Some(comment.clone())
    }

    fn delete_comment(&mut self, comment_id: u64) -> bool {
        self.comments.remove(&comment_id).is_some()
    }

    fn add_sub_issue(&mut self, parent_number: u64, child_number: u64) {
        self.sub_issues
            .entry(parent_number)
            .or_default()
            .push(child_number);
    }

    fn list_sub_issues(&self, parent_number: u64) -> Vec<u64> {
        self.sub_issues
            .get(&parent_number)
            .cloned()
            .unwrap_or_default()
    }

    fn list_issues(&self, label_filter: Option<&str>) -> Vec<&MockIssue> {
        self.issues
            .values()
            .filter(|issue| issue.state == "open")
            .filter(|issue| {
                label_filter
                    .map(|label| issue.labels.iter().any(|current| current == label))
                    .unwrap_or(true)
            })
            .collect()
    }

    fn list_comments(&self, issue_number: u64) -> Vec<&MockComment> {
        self.comments
            .values()
            .filter(|comment| comment.issue_number == issue_number)
            .collect()
    }
}

fn issue_to_json(issue: &MockIssue) -> serde_json::Value {
    serde_json::json!({
        "id": issue.id,
        "number": issue.number,
        "title": issue.title,
        "body": issue.body,
        "state": issue.state,
        "labels": issue.labels.iter().map(|name| serde_json::json!({"id": 1, "name": name})).collect::<Vec<_>>(),
        "created_at": issue.created_at.to_string(),
        "updated_at": issue.updated_at.map(|t| t.to_string()),
        "comments": issue.comments_count,
    })
}

fn comment_to_json(comment: &MockComment) -> serde_json::Value {
    serde_json::json!({
        "id": comment.id,
        "body": comment.body,
        "created_at": comment.created_at.to_string(),
        "updated_at": comment.updated_at.map(|t| t.to_string()),
    })
}

fn issue_body_from_request(body: &serde_json::Value) -> Option<String> {
    body["body"].as_str().map(|s| s.to_string()).or_else(|| {
        body["body"].as_array().map(|parts| {
            let mut out = String::new();
            for part in parts {
                if let Some(s) = part.as_str() {
                    out.push_str(s);
                } else if let Some(n) = part.as_u64() {
                    out.push_str(&n.to_string());
                } else if let Some(map) = part.as_object() {
                    if let Some(v) = map.get("str").and_then(|v| v.as_str()) {
                        out.push_str(v);
                    }
                }
            }
            out
        })
    })
}

fn issue_labels_from_request(body: &serde_json::Value) -> Vec<String> {
    body["labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(|s| s.to_string()))
        .collect()
}

async fn create_mock_store_and_server() -> GitHubPlanStore {
    let state = Arc::new(Mutex::new(MockGitHubState::new()));
    let server = MockServer::start().await;
    mount_mock_handlers(&server, state).await;
    create_test_store_with_server(&server).await
}

async fn create_test_store_with_server(server: &MockServer) -> GitHubPlanStore {
    let config = AuthConfig {
        base_url: server.uri(),
        source: AuthSource::PersonalAccessToken("test-token".to_string()),
    };

    let auth = GitHubAuth::new(config).unwrap();
    let client = GitHubClient::new(auth, "test-owner", "test-repo")
        .await
        .unwrap();

    GitHubPlanStore::new(client)
}

async fn mount_mock_handlers(server: &MockServer, state: Arc<Mutex<MockGitHubState>>) {
    Mock::given(method("GET"))
        .and(wiremock::matchers::path("/repos/test-owner/test-repo"))
        .respond_with(move |_: &Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "full_name": "test-owner/test-repo",
                "private": false,
            }))
        })
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(wiremock::matchers::path(
            "/repos/test-owner/test-repo/labels/harnx-plan",
        ))
        .respond_with(move |_: &Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "harnx-plan",
                "color": "0075ca",
            }))
        })
        .mount(server)
        .await;

    let state_clone = state.clone();
    Mock::given(method("POST"))
        .and(wiremock::matchers::path(
            "/repos/test-owner/test-repo/issues",
        ))
        .respond_with(move |request: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
            let title = body["title"].as_str().unwrap_or("Untitled").to_string();
            let body_text = issue_body_from_request(&body);
            let labels = issue_labels_from_request(&body);

            let mut state = state_clone.lock().unwrap();
            let issue = state.create_issue(title, body_text, labels);

            ResponseTemplate::new(201).set_body_json(issue_to_json(&issue))
        })
        .mount(server)
        .await;

    let state_clone = state.clone();
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(
            r"^/repos/test-owner/test-repo/issues/\d+$",
        ))
        .respond_with(move |request: &Request| {
            let path = request.url.path();
            let number: u64 = path.rsplit('/').next().unwrap().parse().unwrap();

            let state = state_clone.lock().unwrap();
            match state.get_issue(number) {
                Some(issue) => ResponseTemplate::new(200).set_body_json(issue_to_json(issue)),
                None => ResponseTemplate::new(404).set_body_json(serde_json::json!({
                    "message": "Not Found"
                })),
            }
        })
        .mount(server)
        .await;

    let state_clone = state.clone();
    Mock::given(method("PATCH"))
        .and(wiremock::matchers::path_regex(
            r"^/repos/test-owner/test-repo/issues/\d+$",
        ))
        .respond_with(move |request: &Request| {
            let path = request.url.path();
            let number: u64 = path.rsplit('/').next().unwrap().parse().unwrap();

            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
            let title = body["title"].as_str().map(|s| s.to_string());
            let body_text = issue_body_from_request(&body);
            let issue_state = body["state"].as_str().map(|s| s.to_string());

            let mut state = state_clone.lock().unwrap();
            match state.update_issue(number, title, body_text, issue_state) {
                Some(issue) => ResponseTemplate::new(200).set_body_json(issue_to_json(&issue)),
                None => ResponseTemplate::new(404).set_body_json(serde_json::json!({
                    "message": "Not Found"
                })),
            }
        })
        .mount(server)
        .await;

    let state_clone = state.clone();
    Mock::given(method("GET"))
        .and(wiremock::matchers::path(
            "/repos/test-owner/test-repo/issues",
        ))
        .respond_with(move |request: &Request| {
            let label_filter = request
                .url
                .query_pairs()
                .find(|(key, _)| key == "labels")
                .map(|(_, value)| value.into_owned());
            let state = state_clone.lock().unwrap();
            let issues: Vec<_> = state.list_issues(label_filter.as_deref());
            let items: Vec<_> = issues.iter().map(|issue| issue_to_json(issue)).collect();
            ResponseTemplate::new(200).set_body_json(&items)
        })
        .mount(server)
        .await;

    let state_clone = state.clone();
    Mock::given(method("POST"))
        .and(wiremock::matchers::path_regex(
            r"^/repos/test-owner/test-repo/issues/\d+/comments$",
        ))
        .respond_with(move |request: &Request| {
            let path = request.url.path();
            let parts: Vec<&str> = path.split('/').collect();
            let issue_number: u64 = parts[parts.len() - 2].parse().unwrap();

            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
            let body_text = body["body"].as_str().unwrap_or("").to_string();

            let mut state = state_clone.lock().unwrap();
            let comment = state.create_comment(issue_number, body_text);

            ResponseTemplate::new(201).set_body_json(comment_to_json(&comment))
        })
        .mount(server)
        .await;

    let state_clone = state.clone();
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(
            r"^/repos/test-owner/test-repo/issues/comments/\d+$",
        ))
        .respond_with(move |request: &Request| {
            let path = request.url.path();
            let comment_id: u64 = path.rsplit('/').next().unwrap().parse().unwrap();

            let state = state_clone.lock().unwrap();
            match state.get_comment(comment_id) {
                Some(comment) => ResponseTemplate::new(200).set_body_json(comment_to_json(comment)),
                None => ResponseTemplate::new(404).set_body_json(serde_json::json!({
                    "message": "Not Found"
                })),
            }
        })
        .mount(server)
        .await;

    let state_clone = state.clone();
    Mock::given(method("PATCH"))
        .and(wiremock::matchers::path_regex(
            r"^/repos/test-owner/test-repo/issues/comments/\d+$",
        ))
        .respond_with(move |request: &Request| {
            let path = request.url.path();
            let comment_id: u64 = path.rsplit('/').next().unwrap().parse().unwrap();

            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
            let body_text = body["body"].as_str().unwrap_or("").to_string();

            let mut state = state_clone.lock().unwrap();
            match state.update_comment(comment_id, body_text) {
                Some(comment) => {
                    ResponseTemplate::new(200).set_body_json(comment_to_json(&comment))
                }
                None => ResponseTemplate::new(404).set_body_json(serde_json::json!({
                    "message": "Not Found"
                })),
            }
        })
        .mount(server)
        .await;

    let state_clone = state.clone();
    Mock::given(method("DELETE"))
        .and(wiremock::matchers::path_regex(
            r"^/repos/test-owner/test-repo/issues/comments/\d+$",
        ))
        .respond_with(move |request: &Request| {
            let path = request.url.path();
            let comment_id: u64 = path.rsplit('/').next().unwrap().parse().unwrap();

            let mut state = state_clone.lock().unwrap();
            if state.delete_comment(comment_id) {
                ResponseTemplate::new(204)
            } else {
                ResponseTemplate::new(404)
            }
        })
        .mount(server)
        .await;

    let state_clone = state.clone();
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(
            r"^/repos/test-owner/test-repo/issues/\d+/comments$",
        ))
        .respond_with(move |request: &Request| {
            let path = request.url.path();
            let parts: Vec<&str> = path.split('/').collect();
            let issue_number: u64 = parts[parts.len() - 2].parse().unwrap();

            let state = state_clone.lock().unwrap();
            let comments: Vec<_> = state.list_comments(issue_number);
            let items: Vec<_> = comments
                .iter()
                .map(|comment| comment_to_json(comment))
                .collect();

            ResponseTemplate::new(200).set_body_json(&items)
        })
        .mount(server)
        .await;

    let state_clone = state.clone();
    Mock::given(method("POST"))
        .and(wiremock::matchers::path_regex(
            r"^/repos/test-owner/test-repo/issues/\d+/sub_issues$",
        ))
        .respond_with(move |request: &Request| {
            let path = request.url.path();
            let parts: Vec<&str> = path.split('/').collect();
            let parent_number: u64 = parts[parts.len() - 2].parse().unwrap();

            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
            let child_id = body["sub_issue_id"].as_u64().unwrap_or(0);

            let mut state = state_clone.lock().unwrap();
            let child_number = state
                .issues
                .values()
                .find(|issue| issue.id == child_id)
                .map(|issue| issue.number)
                .unwrap_or(child_id);
            state.add_sub_issue(parent_number, child_number);

            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": child_id,
                "number": child_number,
            }))
        })
        .mount(server)
        .await;

    let state_clone = state.clone();
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(
            r"^/repos/test-owner/test-repo/issues/\d+/sub_issues$",
        ))
        .respond_with(move |request: &Request| {
            let path = request.url.path();
            let parts: Vec<&str> = path.split('/').collect();
            let parent_number: u64 = parts[parts.len() - 2].parse().unwrap();

            let state = state_clone.lock().unwrap();
            let sub_numbers = state.list_sub_issues(parent_number);
            let items: Vec<_> = sub_numbers
                .iter()
                .filter_map(|number| state.get_issue(*number))
                .filter(|issue| issue.state == "open")
                .map(issue_to_json)
                .collect();

            ResponseTemplate::new(200).set_body_json(&items)
        })
        .mount(server)
        .await;
}

#[tokio::test]
async fn github_mock_server_works_for_supported_operations() {
    let store = Arc::new(create_mock_store_and_server().await);

    run_conformance(
        store,
        BackendCapabilities {
            preserves_client_id: false,
            deletes_permanently: false,
            rejects_invalid_create_ids: false,
        },
    )
    .await;
}

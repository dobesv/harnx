//! Stateful Wiremock stand-in for the GitHub Issues API.
//!
//! Shared by the conformance suite and the MCP handler tests. It models the
//! behaviour those tests depend on, including rejecting a blank issue title the way
//! GitHub does.

// Each integration test binary compiles this module and uses part of it.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use jiff::Timestamp;
use wiremock::{matchers::method, Mock, MockServer, Request, ResponseTemplate};

use harnx_mcp_plans_github::auth::{AuthConfig, AuthSource, GitHubAuth};
use harnx_mcp_plans_github::client::GitHubClient;
use harnx_mcp_plans_github::store_github::GitHubPlanStore;

#[derive(Debug)]
pub struct MockGitHubState {
    issues: HashMap<u64, MockIssue>,
    comments: HashMap<u64, MockComment>,
    sub_issues: HashMap<u64, Vec<u64>>,
    failing_sub_issue_parents: HashMap<u64, u16>,
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
    pub fn new() -> Self {
        Self {
            issues: HashMap::new(),
            comments: HashMap::new(),
            sub_issues: HashMap::new(),
            failing_sub_issue_parents: HashMap::new(),
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

    fn update_issue(&mut self, number: u64, update: IssueUpdate) -> Option<MockIssue> {
        let issue = self.issues.get_mut(&number)?;
        if let Some(title) = update.title {
            issue.title = title;
        }
        if let Some(body) = update.body {
            issue.body = Some(body);
        }
        if let Some(state) = update.state {
            issue.state = state;
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

    pub fn fail_sub_issue_for_parent(&mut self, parent_number: u64, status: u16) {
        self.failing_sub_issue_parents.insert(parent_number, status);
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
    let body = &body["body"];
    if let Some(text) = body.as_str() {
        return Some(text.to_string());
    }
    let parts = body.as_array()?;
    Some(parts.iter().map(body_part_to_string).collect())
}

/// Render one element of a chunked body, which clients may send as text, a number,
/// or a `{"str": ...}` wrapper.
fn body_part_to_string(part: &serde_json::Value) -> String {
    if let Some(text) = part.as_str() {
        return text.to_string();
    }
    if let Some(number) = part.as_u64() {
        return number.to_string();
    }
    part.get("str")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

/// The trailing path segment as a number, e.g. the issue number in `/issues/7`.
fn trailing_number(request: &Request) -> u64 {
    let path = request.url.path();
    path.rsplit('/')
        .next()
        .and_then(|segment| segment.parse().ok())
        .unwrap_or_else(|| panic!("expected a numeric last path segment in {path}"))
}

/// The number in the second-to-last path segment, e.g. `7` in `/issues/7/comments`.
fn parent_number(request: &Request) -> u64 {
    let path = request.url.path();
    let segments: Vec<&str> = path.split('/').collect();
    segments[segments.len() - 2]
        .parse()
        .unwrap_or_else(|_| panic!("expected a numeric parent path segment in {path}"))
}

/// The requested update to an issue.
struct IssueUpdate {
    title: Option<String>,
    body: Option<String>,
    state: Option<String>,
}

fn request_json(request: &Request) -> serde_json::Value {
    serde_json::from_slice(&request.body).unwrap_or_default()
}

fn not_found() -> ResponseTemplate {
    ResponseTemplate::new(404).set_body_json(serde_json::json!({ "message": "Not Found" }))
}

fn issue_labels_from_request(body: &serde_json::Value) -> Vec<String> {
    body["labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(|s| s.to_string()))
        .collect()
}

pub async fn create_mock_store_and_server() -> GitHubPlanStore {
    let state = Arc::new(Mutex::new(MockGitHubState::new()));
    let server = MockServer::start().await;
    mount_mock_handlers(&server, state).await;
    create_test_store_with_server(&server).await
}

pub async fn create_test_store_with_server(server: &MockServer) -> GitHubPlanStore {
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

pub async fn mount_mock_handlers(server: &MockServer, state: Arc<Mutex<MockGitHubState>>) {
    mount_repo_handlers(server).await;
    mount_issue_write_handlers(server, state.clone()).await;
    mount_issue_read_handlers(server, state.clone()).await;
    mount_comment_write_handlers(server, state.clone()).await;
    mount_comment_read_handlers(server, state.clone()).await;
    mount_sub_issue_handlers(server, state).await;
}

/// Repository and label lookups, which carry no mock state.
async fn mount_repo_handlers(server: &MockServer) {
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
}

/// Create and update issues.
async fn mount_issue_write_handlers(server: &MockServer, state: Arc<Mutex<MockGitHubState>>) {
    let state_clone = state.clone();
    Mock::given(method("POST"))
        .and(wiremock::matchers::path(
            "/repos/test-owner/test-repo/issues",
        ))
        .respond_with(move |request: &Request| {
            let body = request_json(request);
            let title = body["title"].as_str().unwrap_or("").to_string();
            if title.trim().is_empty() {
                return ResponseTemplate::new(422).set_body_json(serde_json::json!({
                    "message": "Validation Failed",
                    "errors": [{
                        "resource": "Issue",
                        "field": "title",
                        "code": "invalid",
                        "message": "title can't be blank",
                    }],
                }));
            }
            let body_text = issue_body_from_request(&body);
            let labels = issue_labels_from_request(&body);

            let mut state = state_clone.lock().unwrap();
            let issue = state.create_issue(title, body_text, labels);

            ResponseTemplate::new(201).set_body_json(issue_to_json(&issue))
        })
        .mount(server)
        .await;
}

/// Read one issue and list issues.
async fn mount_issue_read_handlers(server: &MockServer, state: Arc<Mutex<MockGitHubState>>) {
    let state_clone = state.clone();
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(
            r"^/repos/test-owner/test-repo/issues/\d+$",
        ))
        .respond_with(move |request: &Request| {
            let state = state_clone.lock().unwrap();
            match state.get_issue(trailing_number(request)) {
                Some(issue) => ResponseTemplate::new(200).set_body_json(issue_to_json(issue)),
                None => not_found(),
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
            let body = request_json(request);
            let update = IssueUpdate {
                title: body["title"].as_str().map(|s| s.to_string()),
                body: issue_body_from_request(&body),
                state: body["state"].as_str().map(|s| s.to_string()),
            };

            let mut state = state_clone.lock().unwrap();
            match state.update_issue(trailing_number(request), update) {
                Some(issue) => ResponseTemplate::new(200).set_body_json(issue_to_json(&issue)),
                None => not_found(),
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
}

/// Create, update, and delete issue comments.
async fn mount_comment_write_handlers(server: &MockServer, state: Arc<Mutex<MockGitHubState>>) {
    let state_clone = state.clone();
    Mock::given(method("POST"))
        .and(wiremock::matchers::path_regex(
            r"^/repos/test-owner/test-repo/issues/\d+/comments$",
        ))
        .respond_with(move |request: &Request| {
            let body_text = request_json(request)["body"]
                .as_str()
                .unwrap_or("")
                .to_string();

            let mut state = state_clone.lock().unwrap();
            let comment = state.create_comment(parent_number(request), body_text);

            ResponseTemplate::new(201).set_body_json(comment_to_json(&comment))
        })
        .mount(server)
        .await;
}

/// Read one comment and list an issue's comments.
async fn mount_comment_read_handlers(server: &MockServer, state: Arc<Mutex<MockGitHubState>>) {
    let state_clone = state.clone();
    Mock::given(method("GET"))
        .and(wiremock::matchers::path_regex(
            r"^/repos/test-owner/test-repo/issues/comments/\d+$",
        ))
        .respond_with(move |request: &Request| {
            let state = state_clone.lock().unwrap();
            match state.get_comment(trailing_number(request)) {
                Some(comment) => ResponseTemplate::new(200).set_body_json(comment_to_json(comment)),
                None => not_found(),
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
            let body_text = request_json(request)["body"]
                .as_str()
                .unwrap_or("")
                .to_string();

            let mut state = state_clone.lock().unwrap();
            match state.update_comment(trailing_number(request), body_text) {
                Some(comment) => {
                    ResponseTemplate::new(200).set_body_json(comment_to_json(&comment))
                }
                None => not_found(),
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
            let mut state = state_clone.lock().unwrap();
            if state.delete_comment(trailing_number(request)) {
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
            let state = state_clone.lock().unwrap();
            let comments: Vec<_> = state.list_comments(parent_number(request));
            let items: Vec<_> = comments
                .iter()
                .map(|comment| comment_to_json(comment))
                .collect();

            ResponseTemplate::new(200).set_body_json(&items)
        })
        .mount(server)
        .await;
}

/// Link and list sub-issues.
async fn mount_sub_issue_handlers(server: &MockServer, state: Arc<Mutex<MockGitHubState>>) {
    let state_clone = state.clone();
    Mock::given(method("POST"))
        .and(wiremock::matchers::path_regex(
            r"^/repos/test-owner/test-repo/issues/\d+/sub_issues$",
        ))
        .respond_with(move |request: &Request| {
            let parent = parent_number(request);
            let child_id = request_json(request)["sub_issue_id"].as_u64().unwrap_or(0);

            let mut state = state_clone.lock().unwrap();
            if let Some(status) = state.failing_sub_issue_parents.get(&parent).copied() {
                return ResponseTemplate::new(status);
            }
            let child_number = state
                .issues
                .values()
                .find(|issue| issue.id == child_id)
                .map(|issue| issue.number)
                .unwrap_or(child_id);
            state.add_sub_issue(parent, child_number);

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
            let state = state_clone.lock().unwrap();
            let sub_numbers = state.list_sub_issues(parent_number(request));
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

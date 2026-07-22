use std::collections::HashMap;
use std::sync::Arc;

use std::time::Duration;

use anyhow::{Context, Result};
use harnx_mcp_plans_core::RepoTarget;
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::{GitHubAuth, SystemClock, GITHUB_ACCEPT, GITHUB_API_VERSION, USER_AGENT};
use crate::ratelimit::{
    send_rate_limited, RateLimitConfig, RateLimitExecutor, RequestContext, TokioSleeper,
};

/// Factory for cheap per-repository GitHub clients sharing auth, HTTP, and rate limits.
#[derive(Debug, Clone)]
pub struct GitHubClientFactory {
    auth: GitHubAuth,
    base_url: String,
    raw_http: Client,
    ratelimit: Arc<RateLimitExecutor>,
    default_repo: Option<RepoTarget>,
}

#[derive(Debug, Clone)]
pub struct GitHubClient {
    auth: GitHubAuth,
    owner: String,
    repo: String,
    base_url: String,
    raw_http: Client,
    ratelimit: Arc<RateLimitExecutor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CreateIssue {
    pub title: String,
    pub body: Option<String>,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateIssue {
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ListIssuesParams {
    pub state: Option<String>,
    pub labels: Option<String>,
    pub per_page: Option<u8>,
    pub page: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CreateComment {
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateComment {
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueRecord {
    pub id: u64,
    pub number: u64,
    pub node_id: Option<String>,
    pub state: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub labels: Vec<LabelRecord>,
    pub comments: Option<u64>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueComment {
    pub id: u64,
    pub node_id: Option<String>,
    pub body: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphQlResponse {
    pub data: Option<Value>,
    #[serde(default)]
    pub errors: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GhPage<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CreateIssueRequest<'a> {
    title: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    labels: Option<Vec<&'a str>>,
}

#[derive(Debug, Clone, Serialize, Default)]
struct UpdateIssueRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
struct CreateCommentRequest<'a> {
    body: &'a str,
}

#[derive(Debug, Clone, Serialize)]
struct GraphQlRequest<'a> {
    query: &'a str,
}

#[derive(Debug, Clone, Serialize)]
struct SubIssueMutationRequest {
    sub_issue_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoRecord {
    pub id: u64,
    pub full_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelRecord {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
struct CreateLabelRequest<'a> {
    name: &'a str,
    color: &'a str,
    description: &'a str,
}

impl From<GitHubClient> for GitHubClientFactory {
    fn from(client: GitHubClient) -> Self {
        Self {
            auth: client.auth,
            base_url: client.base_url,
            raw_http: client.raw_http,
            ratelimit: client.ratelimit,
            default_repo: RepoTarget::new(client.owner, client.repo).ok(),
        }
    }
}

impl GitHubClientFactory {
    /// Create a factory with no implicit default repository fallback.
    pub fn new(auth: GitHubAuth, ratelimit: Arc<RateLimitExecutor>) -> Result<Self> {
        let base_url = auth.base_url().to_owned();
        let raw_http = Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .context("build raw GitHub client")?;
        Ok(Self {
            auth,
            base_url,
            raw_http,
            ratelimit,
            default_repo: None,
        })
    }

    /// Return a repo-bound client for a validated target repository.
    pub fn client_for(&self, target: &RepoTarget) -> GitHubClient {
        GitHubClient {
            auth: self.auth.clone(),
            owner: target.owner.clone(),
            repo: target.repo.clone(),
            base_url: self.base_url.clone(),
            raw_http: self.raw_http.clone(),
            ratelimit: self.ratelimit.clone(),
        }
    }

    pub fn default_repo(&self) -> Option<&RepoTarget> {
        self.default_repo.as_ref()
    }
}

impl GitHubClient {
    pub async fn new(
        auth: GitHubAuth,
        owner: impl Into<String>,
        repo: impl Into<String>,
    ) -> Result<Self> {
        Self::with_ratelimit(
            auth,
            owner,
            repo,
            Arc::new(RateLimitExecutor::new(
                Arc::new(SystemClock),
                Arc::new(TokioSleeper),
                RateLimitConfig::default(),
            )),
        )
        .await
    }

    pub async fn with_ratelimit(
        auth: GitHubAuth,
        owner: impl Into<String>,
        repo: impl Into<String>,
        ratelimit: Arc<RateLimitExecutor>,
    ) -> Result<Self> {
        let base_url = auth.base_url().to_owned();
        let raw_http = Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .context("build raw GitHub client")?;
        Ok(Self {
            auth,
            owner: owner.into(),
            repo: repo.into(),
            base_url,
            raw_http,
            ratelimit,
        })
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn repo(&self) -> &str {
        &self.repo
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn ratelimit_executor(&self) -> &Arc<RateLimitExecutor> {
        &self.ratelimit
    }

    pub async fn create_issue(&self, input: CreateIssue) -> Result<IssueRecord> {
        let endpoint = self.repo_endpoint("/issues");
        let issue: IssueRecordWire = self
            .post_json(
                &endpoint,
                &CreateIssueRequest {
                    title: &input.title,
                    body: input.body.as_deref(),
                    labels: if input.labels.is_empty() {
                        None
                    } else {
                        Some(input.labels.iter().map(String::as_str).collect())
                    },
                },
            )
            .await?;
        Ok(map_issue(issue))
    }

    pub async fn get_issue(&self, issue_number: u64) -> Result<IssueRecord> {
        let endpoint = self.repo_endpoint(&format!("/issues/{issue_number}"));
        let issue: IssueRecordWire = self.get_json(&endpoint).await?;
        Ok(map_issue(issue))
    }

    pub async fn update_issue(&self, issue_number: u64, input: UpdateIssue) -> Result<IssueRecord> {
        let endpoint = self.repo_endpoint(&format!("/issues/{issue_number}"));
        let issue: IssueRecordWire = self
            .patch_json(
                &endpoint,
                &UpdateIssueRequest {
                    title: input.title.as_deref(),
                    body: input.body.as_deref(),
                    state: input.state.as_deref().map(normalize_issue_state),
                },
            )
            .await?;
        Ok(map_issue(issue))
    }

    pub async fn close_issue(&self, issue_number: u64) -> Result<IssueRecord> {
        self.update_issue(
            issue_number,
            UpdateIssue {
                state: Some("closed".to_owned()),
                ..UpdateIssue::default()
            },
        )
        .await
    }

    pub async fn list_issues(&self, params: ListIssuesParams) -> Result<GhPage<IssueRecord>> {
        let endpoint = with_query(
            self.repo_endpoint("/issues"),
            params
                .state
                .into_iter()
                .map(|state| ("state", state))
                .chain(params.labels.into_iter().map(|labels| ("labels", labels)))
                .chain(params.per_page.map(|value| ("per_page", value.to_string())))
                .chain(params.page.map(|value| ("page", value.to_string())))
                .collect(),
        );
        let page: GhPage<IssueRecordWire> = self.get_page_json(&endpoint).await?;
        Ok(GhPage {
            items: page.items.into_iter().map(map_issue).collect(),
            next: page.next,
        })
    }

    pub async fn list_issues_next(&self, next_url: &str) -> Result<GhPage<IssueRecord>> {
        let page: GhPage<IssueRecordWire> = self.get_page_json_absolute(next_url).await?;
        Ok(GhPage {
            items: page.items.into_iter().map(map_issue).collect(),
            next: page.next,
        })
    }

    pub async fn create_comment(
        &self,
        issue_number: u64,
        input: CreateComment,
    ) -> Result<IssueComment> {
        let endpoint = self.repo_endpoint(&format!("/issues/{issue_number}/comments"));
        let comment: IssueCommentWire = self
            .post_json(&endpoint, &CreateCommentRequest { body: &input.body })
            .await?;
        Ok(map_comment(comment))
    }

    pub async fn get_comment(&self, comment_id: u64) -> Result<IssueComment> {
        let endpoint = self.repo_endpoint(&format!("/issues/comments/{comment_id}"));
        let comment: IssueCommentWire = self.get_json(&endpoint).await?;
        Ok(map_comment(comment))
    }

    pub async fn update_comment(
        &self,
        comment_id: u64,
        input: UpdateComment,
    ) -> Result<IssueComment> {
        let endpoint = self.repo_endpoint(&format!("/issues/comments/{comment_id}"));
        let comment: IssueCommentWire = self
            .patch_json(&endpoint, &CreateCommentRequest { body: &input.body })
            .await?;
        Ok(map_comment(comment))
    }

    pub async fn list_comments(
        &self,
        issue_number: u64,
        per_page: Option<u8>,
    ) -> Result<GhPage<IssueComment>> {
        let endpoint = with_query(
            self.repo_endpoint(&format!("/issues/{issue_number}/comments")),
            per_page
                .map(|value| vec![("per_page", value.to_string())])
                .unwrap_or_default(),
        );
        let page: GhPage<IssueCommentWire> = self.get_page_json(&endpoint).await?;
        Ok(GhPage {
            items: page.items.into_iter().map(map_comment).collect(),
            next: page.next,
        })
    }

    pub async fn list_comments_next(&self, next_url: &str) -> Result<GhPage<IssueComment>> {
        let page: GhPage<IssueCommentWire> = self.get_page_json_absolute(next_url).await?;
        Ok(GhPage {
            items: page.items.into_iter().map(map_comment).collect(),
            next: page.next,
        })
    }

    /// Delete a comment.
    /// DELETE /repos/{owner}/{repo}/issues/comments/{comment_id}
    pub async fn delete_comment(&self, comment_id: u64) -> Result<()> {
        let endpoint = self.repo_endpoint(&format!("/issues/comments/{comment_id}"));
        let url = self.absolute_url(&endpoint);
        self.request_json_empty::<()>(reqwest::Method::DELETE, &url, None)
            .await
            .context("delete GitHub comment")?;
        Ok(())
    }

    /// Add a sub-issue to a parent issue.
    /// Requires the parent's issue number and the sub-issue's top-level integer issue ID.
    ///
    /// GitHub's sub-issues REST API expects `sub_issue_id` to be the issue's database ID
    /// returned as the top-level `id` field in REST issue responses, not the issue number
    /// and never the opaque `node_id`.
    pub async fn add_sub_issue(
        &self,
        parent_issue_number: u64,
        sub_issue_internal_id: u64,
    ) -> Result<()> {
        let endpoint = self.repo_endpoint(&format!("/issues/{parent_issue_number}/sub_issues"));
        let _: Value = self
            .post_json(
                &endpoint,
                &SubIssueMutationRequest {
                    sub_issue_id: sub_issue_internal_id,
                },
            )
            .await
            .context("add GitHub sub-issue")?;
        Ok(())
    }

    /// Remove a sub-issue from a parent issue.
    pub async fn remove_sub_issue(
        &self,
        parent_issue_number: u64,
        sub_issue_internal_id: u64,
    ) -> Result<()> {
        let endpoint = self.repo_endpoint(&format!("/issues/{parent_issue_number}/sub_issue"));
        self.request_json::<Value, _>(
            reqwest::Method::DELETE,
            &endpoint,
            Some(&serde_json::json!({ "sub_issue_id": sub_issue_internal_id })),
        )
        .await
        .context("remove GitHub sub-issue")?;
        Ok(())
    }

    pub async fn get_repository(&self) -> Result<RepoRecord> {
        let endpoint = self.repo_endpoint("");
        self.get_json(&endpoint)
            .await
            .context("get GitHub repository")
    }

    pub async fn get_label(&self, label_name: &str) -> Result<LabelRecord> {
        let endpoint = self.repo_endpoint(&format!(
            "/labels/{}",
            Self::encode_path_segment(label_name)
        ));
        self.get_json(&endpoint).await.context("get GitHub label")
    }

    pub async fn create_label(
        &self,
        label_name: &str,
        color: &str,
        description: &str,
    ) -> Result<LabelRecord> {
        let endpoint = self.repo_endpoint("/labels");
        self.post_json(
            &endpoint,
            &CreateLabelRequest {
                name: label_name,
                color,
                description,
            },
        )
        .await
        .context("create GitHub label")
    }

    pub async fn ensure_label(
        &self,
        label_name: &str,
        color: &str,
        description: &str,
    ) -> Result<()> {
        match self.get_label(label_name).await {
            Ok(_) => Ok(()),
            Err(err) => {
                let err_text = err.to_string().to_lowercase();
                if !(err_text.contains("404") || err_text.contains("not found")) {
                    return Err(err.context("check GitHub label"));
                }
                match self.create_label(label_name, color, description).await {
                    Ok(_) => Ok(()),
                    Err(create_err) => {
                        let create_text = create_err.to_string().to_lowercase();
                        if create_text.contains("422") || create_text.contains("already_exists") {
                            Ok(())
                        } else {
                            Err(create_err.context("ensure GitHub label"))
                        }
                    }
                }
            }
        }
    }
    pub async fn list_sub_issues(&self, parent_issue_number: u64) -> Result<GhPage<Value>> {
        let endpoint = self.repo_endpoint(&format!("/issues/{parent_issue_number}/sub_issues"));
        self.get_page_json(&endpoint)
            .await
            .context("list GitHub sub-issues")
    }

    pub async fn list_sub_issues_next(&self, next_url: &str) -> Result<GhPage<Value>> {
        self.get_page_json_absolute(next_url).await
    }

    pub async fn post_graphql(&self, query: &str) -> Result<GraphQlResponse> {
        self.post_json("/graphql", &GraphQlRequest { query })
            .await
            .context("GitHub GraphQL request")
    }

    async fn get_json<T>(&self, endpoint: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        self.request_json(
            reqwest::Method::GET,
            &self.absolute_url(endpoint),
            Option::<&Value>::None,
        )
        .await
    }

    async fn get_page_json<T>(&self, endpoint: &str) -> Result<GhPage<T>>
    where
        T: DeserializeOwned,
    {
        self.get_page_json_absolute(&self.absolute_url(endpoint))
            .await
    }

    async fn get_page_json_absolute<T>(&self, url: &str) -> Result<GhPage<T>>
    where
        T: DeserializeOwned,
    {
        let response = self
            .send_url(reqwest::Method::GET, url, Option::<Value>::None)
            .await?;
        decode_page(response).await
    }

    async fn post_json<T, B>(&self, endpoint: &str, body: &B) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        self.request_json(
            reqwest::Method::POST,
            &self.absolute_url(endpoint),
            Some(body),
        )
        .await
    }

    async fn patch_json<T, B>(&self, endpoint: &str, body: &B) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        self.request_json(
            reqwest::Method::PATCH,
            &self.absolute_url(endpoint),
            Some(body),
        )
        .await
    }

    async fn request_json<T, B>(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&B>,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let body_value = body
            .map(serde_json::to_value)
            .transpose()
            .context("serialize GitHub request body")?;
        let response = self.send_url(method, url, body_value).await?;
        response.json::<T>().await.context("decode GitHub response")
    }

    async fn request_json_empty<B>(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&B>,
    ) -> Result<()>
    where
        B: Serialize + ?Sized,
    {
        let body_value = body
            .map(serde_json::to_value)
            .transpose()
            .context("serialize GitHub request body")?;
        let response = self.send_url(method, url, body_value).await?;
        // GitHub returns 204 No Content for successful deletes
        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(());
        }
        // Consume response body to avoid connection leak
        let _ = response.bytes().await;
        Ok(())
    }

    async fn send_url(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<Value>,
    ) -> Result<reqwest::Response> {
        let context = RequestContext::new(method.clone(), url.to_owned());
        send_rate_limited(&self.ratelimit, context, || {
            let token = self.auth.clone();
            let client = self.raw_http.clone();
            let method = method.clone();
            let url = url.to_owned();
            let body = body.clone();
            async move {
                let bearer = token.bearer_token().await?;
                let mut request = client
                    .request(method, url)
                    .header("Accept", GITHUB_ACCEPT)
                    .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
                    .bearer_auth(bearer);
                if let Some(body) = body {
                    request = request.json(&body);
                }
                request.send().await.map_err(Into::into)
            }
        })
        .await
    }

    fn encode_path_segment(value: &str) -> String {
        value.replace('/', "%2F").replace('\\', "%5C")
    }

    fn absolute_url(&self, endpoint: &str) -> String {
        format!("{}{}", self.base_url, endpoint)
    }

    fn repo_endpoint(&self, suffix: &str) -> String {
        format!(
            "/repos/{}/{}{}",
            Self::encode_path_segment(&self.owner),
            Self::encode_path_segment(&self.repo),
            suffix
        )
    }
}

fn normalize_issue_state(value: &str) -> &str {
    match value {
        "open" => "open",
        "closed" => "closed",
        other => other,
    }
}

fn with_query(mut endpoint: String, query: Vec<(&str, String)>) -> String {
    if query.is_empty() {
        return endpoint;
    }
    endpoint.push('?');
    endpoint.push_str(
        &query
            .into_iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("&"),
    );
    endpoint
}

async fn decode_page<T>(response: reqwest::Response) -> Result<GhPage<T>>
where
    T: DeserializeOwned,
{
    let next = parse_next_link(response.headers());
    let items = response
        .json::<Vec<T>>()
        .await
        .context("decode GitHub page items")?;
    Ok(GhPage { items, next })
}

fn parse_next_link(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let value = headers.get(reqwest::header::LINK)?.to_str().ok()?;
    let links = parse_link_header(value);
    links.get("next").cloned()
}

fn parse_link_header(header: &str) -> HashMap<String, String> {
    let mut parsed = HashMap::new();
    for part in header.split(',') {
        let trimmed = part.trim();
        let Some((url_part, rel_part)) = trimmed.split_once(';') else {
            continue;
        };
        let url = url_part
            .trim()
            .trim_start_matches('<')
            .trim_end_matches('>');
        let rel = rel_part.trim();
        let Some(rel_name) = rel
            .strip_prefix("rel=\"")
            .and_then(|value| value.strip_suffix('"'))
        else {
            continue;
        };
        parsed.insert(rel_name.to_owned(), url.to_owned());
    }
    parsed
}

fn map_issue(issue: IssueRecordWire) -> IssueRecord {
    IssueRecord {
        id: issue.id,
        number: issue.number,
        node_id: issue.node_id,
        state: issue.state,
        title: Some(issue.title),
        body: issue.body,
        labels: issue.labels,
        comments: Some(issue.comments),
        created_at: issue.created_at,
        updated_at: issue.updated_at,
    }
}

fn map_comment(comment: IssueCommentWire) -> IssueComment {
    IssueComment {
        id: comment.id,
        node_id: comment.node_id,
        body: comment.body,
        created_at: comment.created_at,
        updated_at: comment.updated_at,
    }
}

#[derive(Debug, Clone, Deserialize)]
struct IssueRecordWire {
    id: u64,
    number: u64,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    state: Option<String>,
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    labels: Vec<LabelRecord>,
    comments: u64,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct IssueCommentWire {
    id: u64,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

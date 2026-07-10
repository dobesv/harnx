//! GitHub Issues-backed implementation of `PlanStore`.
//!
//! This module implements the `PlanStore` trait using GitHub Issues:
//! - Plans = GitHub issues (labelled with plan label)
//! - Tasks = Sub-issues of plan issues
//! - Notes = Comments on plan issues
//!
//! ## ID Mapping
//! - Plan/Task IDs = stringified GitHub issue numbers
//! - Note IDs = stringified GitHub comment IDs
//!
//! ## De-duplication
//! Multiple issues/comments can share the same client-provided ID (stored in front-matter).
//! Read operations resolve duplicates by keeping the most recently `updated_at` entry.
//!
//! ## Pagination
//! `PageToken` encodes the GitHub `Link` header's `next` URL as an opaque cursor.
//!
//! ## Delete Behavior
//! - Plan/Task delete = close the issue (configurable behavior)
//! - Note delete = delete the comment via GitHub API

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use harnx_mcp_plans_core::{
    NewNote, NewPlan, NewTask, Note, NoteId, NoteMetaUpdate, Page, PageToken, Plan, PlanId,
    PlanMetaUpdate, PlanStore, StoreError, Task, TaskFilter, TaskId, TaskMetaUpdate,
};
use jiff::Timestamp;

use crate::client::{
    CreateComment, CreateIssue, GitHubClient, IssueRecord, ListIssuesParams, UpdateComment,
    UpdateIssue,
};
use crate::codec::{
    comment_to_note, issue_to_plan, issue_to_task, new_note_to_comment, new_plan_to_issue,
    new_task_to_issue, note_meta_update_to_comment_body, plan_meta_update_to_issue_body,
    task_meta_update_to_issue_body, DecodedNote, DecodedPlan, DecodedTask,
};

/// Maximum number of sub-issues allowed per parent issue.
const MAX_SUB_ISSUES: usize = 100;

/// Configuration for `GitHubPlanStore`.
#[derive(Debug, Clone)]
pub struct GitHubStoreConfig {
    /// Label used to identify plan issues.
    /// Default: "harnx-plan"
    pub plan_label: String,
    /// Whether to close issues on delete (true) or just leave them (false).
    /// Default: true (close)
    pub delete_is_close: bool,
}

impl Default for GitHubStoreConfig {
    fn default() -> Self {
        Self {
            plan_label: "harnx-plan".to_string(),
            delete_is_close: true,
        }
    }
}

/// GitHub-backed `PlanStore` implementation.
#[derive(Debug, Clone)]
pub struct GitHubPlanStore {
    client: GitHubClient,
    config: GitHubStoreConfig,
}

impl GitHubPlanStore {
    /// Create a new store with default configuration.
    pub fn new(client: GitHubClient) -> Self {
        Self {
            client,
            config: GitHubStoreConfig::default(),
        }
    }

    /// Create a new store with the given client and config.
    pub fn with_config(client: GitHubClient, config: GitHubStoreConfig) -> Self {
        Self { client, config }
    }

    pub fn client_ref(&self) -> &GitHubClient {
        &self.client
    }

    pub fn config_ref(&self) -> &GitHubStoreConfig {
        &self.config
    }

    /// Parse a Plan ID (string) to an issue number (u64).
    fn parse_plan_id(plan_id: &PlanId) -> Result<u64, StoreError> {
        plan_id.parse::<u64>().map_err(|_| StoreError::NotFound)
    }

    /// Parse a Task ID (string) to an issue number (u64).
    fn parse_task_id(task_id: &TaskId) -> Result<u64, StoreError> {
        task_id.parse::<u64>().map_err(|_| StoreError::NotFound)
    }

    /// Parse a Note ID (string) to a comment ID (u64).
    fn parse_note_id(note_id: &NoteId) -> Result<u64, StoreError> {
        note_id.parse::<u64>().map_err(|_| StoreError::NotFound)
    }

    fn sort_decoded_plans(plans: &mut [DecodedPlan]) {
        plans.sort_by_key(|plan| plan.plan.id.parse::<u64>().unwrap_or(u64::MAX));
    }

    fn sort_decoded_tasks(tasks: &mut [DecodedTask]) {
        tasks.sort_by_key(|task| task.task.id.parse::<u64>().unwrap_or(u64::MAX));
    }

    fn sort_decoded_notes(notes: &mut [DecodedNote]) {
        notes.sort_by_key(|note| note.note.id.parse::<u64>().unwrap_or(u64::MAX));
    }

    fn is_better_plan_candidate(existing: &DecodedPlan, candidate: &DecodedPlan) -> bool {
        match (existing.plan.updated_at, candidate.plan.updated_at) {
            (Some(existing_ts), Some(candidate_ts)) if candidate_ts != existing_ts => {
                candidate_ts > existing_ts
            }
            (Some(_), None) => false,
            (None, Some(_)) => true,
            _ => {
                candidate.plan.id.parse::<u64>().unwrap_or(0)
                    > existing.plan.id.parse::<u64>().unwrap_or(0)
            }
        }
    }

    fn is_better_task_candidate(existing: &DecodedTask, candidate: &DecodedTask) -> bool {
        match (existing.task.updated_at, candidate.task.updated_at) {
            (Some(existing_ts), Some(candidate_ts)) if candidate_ts != existing_ts => {
                candidate_ts > existing_ts
            }
            (Some(_), None) => false,
            (None, Some(_)) => true,
            _ => {
                candidate.task.id.parse::<u64>().unwrap_or(0)
                    > existing.task.id.parse::<u64>().unwrap_or(0)
            }
        }
    }

    fn is_better_note_candidate(existing: &DecodedNote, candidate: &DecodedNote) -> bool {
        match (existing.note.updated_at, candidate.note.updated_at) {
            (Some(existing_ts), Some(candidate_ts)) if candidate_ts != existing_ts => {
                candidate_ts > existing_ts
            }
            (Some(_), None) => false,
            (None, Some(_)) => true,
            _ => {
                candidate.note.id.parse::<u64>().unwrap_or(0)
                    > existing.note.id.parse::<u64>().unwrap_or(0)
            }
        }
    }

    async fn ensure_issue_is_plan(&self, plan_number: u64) -> Result<IssueRecord, StoreError> {
        let issue = self
            .client
            .get_issue(plan_number)
            .await
            .map_err(map_github_error)?;
        if issue
            .labels
            .iter()
            .any(|label| label.name == self.config.plan_label)
        {
            Ok(issue)
        } else {
            Err(StoreError::NotFound)
        }
    }

    async fn count_sub_issues(&self, plan_number: u64) -> Result<usize, StoreError> {
        let mut total = 0usize;
        let mut page = self
            .client
            .list_sub_issues(plan_number)
            .await
            .map_err(map_github_error)?;
        loop {
            total += page.items.len();
            let Some(next) = page.next.take() else {
                break;
            };
            page = self
                .client
                .list_sub_issues_next(&next)
                .await
                .map_err(map_github_error)?;
        }
        Ok(total)
    }

    async fn find_task_membership(
        &self,
        plan_number: u64,
        task_number: u64,
    ) -> Result<bool, StoreError> {
        let mut page = self
            .client
            .list_sub_issues(plan_number)
            .await
            .map_err(map_github_error)?;
        loop {
            if page
                .items
                .iter()
                .filter_map(|value| value.get("number").and_then(|n| n.as_u64()))
                .any(|number| number == task_number)
            {
                return Ok(true);
            }
            let Some(next) = page.next.take() else {
                return Ok(false);
            };
            page = self
                .client
                .list_sub_issues_next(&next)
                .await
                .map_err(map_github_error)?;
        }
    }

    async fn find_note_membership(
        &self,
        plan_number: u64,
        comment_id: u64,
    ) -> Result<bool, StoreError> {
        let mut page = self
            .client
            .list_comments(plan_number, Some(100))
            .await
            .map_err(map_github_error)?;
        loop {
            if page.items.iter().any(|existing| existing.id == comment_id) {
                return Ok(true);
            }
            let Some(next) = page.next.take() else {
                return Ok(false);
            };
            page = self
                .client
                .list_comments_next(&next)
                .await
                .map_err(map_github_error)?;
        }
    }

    /// Apply read-side duplicate resolution for tasks.
    /// Groups by client_id and keeps the most recently updated entry.
    fn dedupe_tasks(tasks: Vec<DecodedTask>) -> Vec<DecodedTask> {
        let mut by_client_id: HashMap<Option<String>, DecodedTask> = HashMap::new();

        for task in tasks {
            let key = task.client_id.clone();
            let entry = by_client_id.entry(key).or_insert_with(|| task.clone());

            let should_replace = Self::is_better_task_candidate(entry, &task);

            if should_replace {
                *entry = task;
            }
        }

        let mut deduped: Vec<_> = by_client_id.into_values().collect();
        Self::sort_decoded_tasks(&mut deduped);
        deduped
    }

    /// Apply read-side duplicate resolution for notes.
    /// Groups by client_id and keeps the most recently updated entry.
    fn dedupe_notes(notes: Vec<DecodedNote>) -> Vec<DecodedNote> {
        let mut by_client_id: HashMap<Option<String>, DecodedNote> = HashMap::new();

        for note in notes {
            let key = note.client_id.clone();
            let entry = by_client_id.entry(key).or_insert_with(|| note.clone());

            let should_replace = Self::is_better_note_candidate(entry, &note);

            if should_replace {
                *entry = note;
            }
        }

        let mut deduped: Vec<_> = by_client_id.into_values().collect();
        Self::sort_decoded_notes(&mut deduped);
        deduped
    }

    /// Decode an IssueRecord to DecodedPlan.
    fn decode_issue_to_plan(issue: IssueRecord) -> Result<DecodedPlan, StoreError> {
        let created_at = parse_github_timestamp(issue.created_at.as_deref().unwrap_or(""))?;
        let updated_at = issue
            .updated_at
            .as_deref()
            .map(parse_github_timestamp)
            .transpose()?;

        Ok(issue_to_plan(
            issue.number,
            issue.title.as_deref().unwrap_or(""),
            issue.body.as_deref(),
            created_at,
            updated_at,
        ))
    }

    /// Decode an IssueRecord to DecodedTask.
    fn decode_issue_to_task(issue: IssueRecord, plan_id: &str) -> Result<DecodedTask, StoreError> {
        let created_at = parse_github_timestamp(issue.created_at.as_deref().unwrap_or(""))?;
        let updated_at = issue
            .updated_at
            .as_deref()
            .map(parse_github_timestamp)
            .transpose()?;

        Ok(issue_to_task(
            plan_id.to_string(),
            issue.number,
            issue.title.as_deref().unwrap_or(""),
            issue.body.as_deref(),
            created_at,
            updated_at,
        ))
    }

    /// Decode an IssueComment to DecodedNote.
    fn decode_comment_to_note(
        comment: crate::client::IssueComment,
    ) -> Result<DecodedNote, StoreError> {
        let created_at = parse_github_timestamp(comment.created_at.as_deref().unwrap_or(""))?;
        let updated_at = comment
            .updated_at
            .as_deref()
            .map(parse_github_timestamp)
            .transpose()?;

        Ok(comment_to_note(
            comment.id,
            comment.body.as_deref(),
            created_at,
            updated_at,
        ))
    }

    async fn ensure_task_belongs_to_plan(
        &self,
        plan: &PlanId,
        task: &TaskId,
    ) -> Result<u64, StoreError> {
        let plan_number = Self::parse_plan_id(plan)?;
        self.ensure_issue_is_plan(plan_number).await?;
        let task_number = Self::parse_task_id(task)?;

        if self.find_task_membership(plan_number, task_number).await? {
            Ok(task_number)
        } else {
            Err(StoreError::NotFound)
        }
    }

    async fn ensure_note_belongs_to_plan(
        &self,
        plan: &PlanId,
        note: &NoteId,
    ) -> Result<crate::client::IssueComment, StoreError> {
        let plan_number = Self::parse_plan_id(plan)?;
        self.ensure_issue_is_plan(plan_number).await?;
        let comment_id = Self::parse_note_id(note)?;
        let comment = self
            .client
            .get_comment(comment_id)
            .await
            .map_err(map_github_error)?;

        if self.find_note_membership(plan_number, comment_id).await? {
            Ok(comment)
        } else {
            Err(StoreError::NotFound)
        }
    }
}

/// Parse a GitHub timestamp string to jiff::Timestamp.
fn parse_github_timestamp(s: &str) -> Result<Timestamp, StoreError> {
    s.parse::<Timestamp>()
        .map_err(|e| StoreError::Backend(anyhow!("failed to parse timestamp '{}': {}", s, e)))
}

/// Map GitHub client errors to StoreError.
fn map_github_error(e: anyhow::Error) -> StoreError {
    let err_str = e.to_string().to_lowercase();

    if err_str.contains("404") || err_str.contains("not found") {
        StoreError::NotFound
    } else if err_str.contains("422") || err_str.contains("validation") {
        StoreError::InvalidParams(e.to_string())
    } else if err_str.contains("rate limit") {
        StoreError::RateLimited {
            retry_after_secs: 30,
        }
    } else {
        StoreError::Backend(e)
    }
}

#[async_trait]
impl PlanStore for GitHubPlanStore {
    async fn list_plans(&self, page: Option<PageToken>) -> Result<Page<Plan>, StoreError> {
        let gh_page = match page {
            Some(token) => self.client.list_issues_next(&token.0).await,
            None => {
                self.client
                    .list_issues(ListIssuesParams {
                        state: Some("open".to_string()),
                        labels: Some(self.config.plan_label.clone()),
                        per_page: Some(100),
                        page: None,
                    })
                    .await
            }
        }
        .map_err(map_github_error)?;

        let filtered: Vec<IssueRecord> = gh_page
            .items
            .into_iter()
            .filter(|issue| {
                issue
                    .labels
                    .iter()
                    .any(|label| label.name == self.config.plan_label)
            })
            .collect();

        let mut plans: Vec<DecodedPlan> = filtered
            .into_iter()
            .map(Self::decode_issue_to_plan)
            .collect::<Result<Vec<_>, _>>()?;

        let mut by_client_id: HashMap<Option<String>, DecodedPlan> = HashMap::new();
        for plan in plans {
            let key = plan.client_id.clone();
            let entry = by_client_id.entry(key).or_insert_with(|| plan.clone());

            let should_replace = Self::is_better_plan_candidate(entry, &plan);

            if should_replace {
                *entry = plan;
            }
        }
        plans = by_client_id.into_values().collect();
        Self::sort_decoded_plans(&mut plans);

        Ok(Page {
            items: plans.into_iter().map(|dp| dp.plan).collect(),
            next: gh_page.next.map(PageToken),
        })
    }

    async fn get_plan(&self, plan: &PlanId) -> Result<Plan, StoreError> {
        let issue_number = Self::parse_plan_id(plan)?;
        let issue = self.ensure_issue_is_plan(issue_number).await?;

        let decoded = Self::decode_issue_to_plan(issue)?;
        Ok(decoded.plan)
    }

    async fn add_plan(&self, new_plan: NewPlan) -> Result<Plan, StoreError> {
        let (title, body) = new_plan_to_issue(&new_plan, None, "");

        let issue = self
            .client
            .create_issue(CreateIssue {
                title,
                body: Some(body),
                labels: vec![self.config.plan_label.clone()],
            })
            .await
            .map_err(map_github_error)?;

        Ok(Plan {
            id: issue.number.to_string(),
            title: new_plan.title,
            summary: new_plan.summary,
            author: new_plan.author,
            assignee: new_plan.assignee,
            executor: new_plan.executor,
            git_branch: new_plan.git_branch,
            github_owner_repo: new_plan.github_owner_repo,
            created_at: Timestamp::now(),
            updated_at: None,
        })
    }

    async fn update_plan_meta(
        &self,
        plan: &PlanId,
        update: PlanMetaUpdate,
    ) -> Result<Plan, StoreError> {
        let issue_number = Self::parse_plan_id(plan)?;
        let issue = self.ensure_issue_is_plan(issue_number).await?;
        let decoded = Self::decode_issue_to_plan(issue)?;

        let (new_title_opt, new_body) = plan_meta_update_to_issue_body(&decoded, &update);

        let updated = self
            .client
            .update_issue(
                issue_number,
                UpdateIssue {
                    title: new_title_opt,
                    body: Some(new_body),
                    state: None,
                },
            )
            .await
            .map_err(map_github_error)?;

        let decoded = Self::decode_issue_to_plan(updated)?;
        Ok(decoded.plan)
    }

    async fn delete_plan(&self, plan: &PlanId) -> Result<(), StoreError> {
        if self.config.delete_is_close {
            let issue_number = Self::parse_plan_id(plan)?;
            self.ensure_issue_is_plan(issue_number).await?;
            self.client
                .close_issue(issue_number)
                .await
                .map_err(map_github_error)?;
            Ok(())
        } else {
            Err(StoreError::InvalidParams(
                "delete behavior is configured to leave plan issues open".to_string(),
            ))
        }
    }

    async fn read_plan_body(&self, plan: &PlanId) -> Result<String, StoreError> {
        let issue_number = Self::parse_plan_id(plan)?;
        let issue = self.ensure_issue_is_plan(issue_number).await?;
        let decoded = Self::decode_issue_to_plan(issue)?;
        Ok(decoded.body)
    }

    async fn write_plan_body(&self, plan: &PlanId, body: &str) -> Result<(), StoreError> {
        let issue_number = Self::parse_plan_id(plan)?;
        let issue = self.ensure_issue_is_plan(issue_number).await?;
        let decoded = Self::decode_issue_to_plan(issue)?;

        let (title_opt, final_body) = plan_meta_update_to_issue_body(
            &DecodedPlan {
                plan: decoded.plan.clone(),
                body: body.to_string(),
                client_id: decoded.client_id.clone(),
                jira_key: decoded.jira_key.clone(),
            },
            &PlanMetaUpdate::default(),
        );

        self.client
            .update_issue(
                issue_number,
                UpdateIssue {
                    title: title_opt,
                    body: Some(final_body),
                    state: None,
                },
            )
            .await
            .map_err(map_github_error)?;

        Ok(())
    }

    async fn list_tasks(
        &self,
        plan: &PlanId,
        filter: TaskFilter,
        page: Option<PageToken>,
    ) -> Result<Page<Task>, StoreError> {
        let plan_number = Self::parse_plan_id(plan)?;

        let gh_page = match page {
            Some(token) => self.client.list_sub_issues_next(&token.0).await,
            None => self.client.list_sub_issues(plan_number).await,
        }
        .map_err(map_github_error)?;

        let mut tasks: Vec<DecodedTask> = gh_page
            .items
            .into_iter()
            .filter_map(|value| {
                let number = value.get("number")?.as_u64()?;
                let title = value.get("title")?.as_str()?;
                let body = value.get("body").and_then(|b| b.as_str());
                let created_at_str = value.get("created_at")?.as_str().unwrap_or("");
                let updated_at_str = value.get("updated_at").and_then(|v| v.as_str());

                let created_at = parse_github_timestamp(created_at_str).ok()?;
                let updated_at = updated_at_str.and_then(|s| parse_github_timestamp(s).ok());

                Some(issue_to_task(
                    plan.clone(),
                    number,
                    title,
                    body,
                    created_at,
                    updated_at,
                ))
            })
            .collect();

        if let Some(status) = filter.status {
            tasks.retain(|dt| dt.task.status == status);
        }
        if let Some(tag) = filter.tag {
            tasks.retain(|dt| dt.task.tags.contains(&tag));
        }

        tasks = Self::dedupe_tasks(tasks);

        Ok(Page {
            items: tasks.into_iter().map(|dt| dt.task).collect(),
            next: gh_page.next.map(PageToken),
        })
    }

    async fn get_task(&self, plan: &PlanId, task: &TaskId) -> Result<Task, StoreError> {
        let task_number = self.ensure_task_belongs_to_plan(plan, task).await?;

        let issue = self
            .client
            .get_issue(task_number)
            .await
            .map_err(map_github_error)?;

        let decoded = Self::decode_issue_to_task(issue, plan)?;
        Ok(decoded.task)
    }

    async fn add_task(&self, plan: &PlanId, new_task: NewTask) -> Result<Task, StoreError> {
        let plan_number = Self::parse_plan_id(plan)?;

        self.ensure_issue_is_plan(plan_number).await?;
        let current_sub_count = self.count_sub_issues(plan_number).await?;
        if current_sub_count >= MAX_SUB_ISSUES {
            return Err(StoreError::InvalidParams(format!(
                "plan {} already has maximum number of sub-issues ({})",
                plan, MAX_SUB_ISSUES
            )));
        }

        let (title, body) = new_task_to_issue(plan, &new_task, None, "");

        let issue = self
            .client
            .create_issue(CreateIssue {
                title,
                body: Some(body),
                labels: Vec::new(),
            })
            .await
            .map_err(map_github_error)?;

        let task_number = issue.number;
        let task_internal_id = issue.id;

        self.client
            .add_sub_issue(plan_number, task_internal_id)
            .await
            .map_err(map_github_error)?;

        Ok(Task {
            id: task_number.to_string(),
            title: new_task.title,
            summary: new_task.summary,
            author: new_task.author,
            assignee: new_task.assignee,
            executor: new_task.executor,
            tags: new_task.tags,
            plan: plan.clone(),
            status: new_task.status.unwrap_or_else(|| "open".to_string()),
            created_at: Timestamp::now(),
            updated_at: None,
            dependencies: new_task.dependencies,
        })
    }

    async fn update_task_meta(
        &self,
        plan: &PlanId,
        task: &TaskId,
        update: TaskMetaUpdate,
    ) -> Result<Task, StoreError> {
        let task_number = self.ensure_task_belongs_to_plan(plan, task).await?;

        let issue = self
            .client
            .get_issue(task_number)
            .await
            .map_err(map_github_error)?;
        let decoded = Self::decode_issue_to_task(issue, plan)?;

        let (new_title_opt, new_body) = task_meta_update_to_issue_body(&decoded, &update);

        let updated = self
            .client
            .update_issue(
                task_number,
                UpdateIssue {
                    title: new_title_opt,
                    body: Some(new_body),
                    state: None,
                },
            )
            .await
            .map_err(map_github_error)?;

        let decoded = Self::decode_issue_to_task(updated, plan)?;
        Ok(decoded.task)
    }

    async fn delete_task(&self, plan: &PlanId, task: &TaskId) -> Result<(), StoreError> {
        if self.config.delete_is_close {
            let task_number = self.ensure_task_belongs_to_plan(plan, task).await?;
            self.client
                .close_issue(task_number)
                .await
                .map_err(map_github_error)?;
            Ok(())
        } else {
            Err(StoreError::InvalidParams(
                "delete behavior is configured to leave task issues open".to_string(),
            ))
        }
    }

    async fn read_task_body(&self, plan: &PlanId, task: &TaskId) -> Result<String, StoreError> {
        let task_number = self.ensure_task_belongs_to_plan(plan, task).await?;
        let issue = self
            .client
            .get_issue(task_number)
            .await
            .map_err(map_github_error)?;
        let decoded = Self::decode_issue_to_task(issue, plan)?;
        Ok(decoded.body)
    }

    async fn write_task_body(
        &self,
        plan: &PlanId,
        task: &TaskId,
        body: &str,
    ) -> Result<(), StoreError> {
        let task_number = self.ensure_task_belongs_to_plan(plan, task).await?;

        let issue = self
            .client
            .get_issue(task_number)
            .await
            .map_err(map_github_error)?;
        let decoded = Self::decode_issue_to_task(issue, plan)?;

        let (title_opt, final_body) = task_meta_update_to_issue_body(
            &DecodedTask {
                task: decoded.task.clone(),
                body: body.to_string(),
                client_id: decoded.client_id.clone(),
                jira_key: decoded.jira_key.clone(),
            },
            &TaskMetaUpdate::default(),
        );

        self.client
            .update_issue(
                task_number,
                UpdateIssue {
                    title: title_opt,
                    body: Some(final_body),
                    state: None,
                },
            )
            .await
            .map_err(map_github_error)?;

        Ok(())
    }

    async fn list_notes(
        &self,
        plan: &PlanId,
        page: Option<PageToken>,
    ) -> Result<Page<Note>, StoreError> {
        let plan_number = Self::parse_plan_id(plan)?;

        let gh_page = match page {
            Some(token) => self.client.list_comments_next(&token.0).await,
            None => self.client.list_comments(plan_number, Some(100)).await,
        }
        .map_err(map_github_error)?;

        let mut notes: Vec<DecodedNote> = gh_page
            .items
            .into_iter()
            .map(Self::decode_comment_to_note)
            .collect::<Result<Vec<_>, _>>()?;

        notes = Self::dedupe_notes(notes);

        Ok(Page {
            items: notes.into_iter().map(|dn| dn.note).collect(),
            next: gh_page.next.map(PageToken),
        })
    }

    async fn get_note(&self, plan: &PlanId, note: &NoteId) -> Result<Note, StoreError> {
        let comment = self.ensure_note_belongs_to_plan(plan, note).await?;
        let decoded = Self::decode_comment_to_note(comment)?;
        Ok(decoded.note)
    }

    async fn add_note(&self, plan: &PlanId, new_note: NewNote) -> Result<Note, StoreError> {
        let plan_number = Self::parse_plan_id(plan)?;

        let body = new_note_to_comment(&new_note, "");

        let comment = self
            .client
            .create_comment(plan_number, CreateComment { body })
            .await
            .map_err(map_github_error)?;

        Ok(Note {
            id: comment.id.to_string(),
            summary: new_note.summary,
            author: new_note.author,
            created_at: Timestamp::now(),
            updated_at: None,
        })
    }

    async fn update_note_meta(
        &self,
        plan: &PlanId,
        note: &NoteId,
        update: NoteMetaUpdate,
    ) -> Result<Note, StoreError> {
        let comment = self.ensure_note_belongs_to_plan(plan, note).await?;
        let decoded = Self::decode_comment_to_note(comment)?;
        let comment_id = Self::parse_note_id(note)?;

        let new_body = note_meta_update_to_comment_body(&decoded, &update);

        let updated = self
            .client
            .update_comment(comment_id, UpdateComment { body: new_body })
            .await
            .map_err(map_github_error)?;

        let decoded = Self::decode_comment_to_note(updated)?;
        Ok(decoded.note)
    }

    async fn delete_note(&self, plan: &PlanId, note: &NoteId) -> Result<(), StoreError> {
        let comment = self.ensure_note_belongs_to_plan(plan, note).await?;
        self.client
            .delete_comment(comment.id)
            .await
            .map_err(map_github_error)?;

        Ok(())
    }

    async fn read_note_body(&self, plan: &PlanId, note: &NoteId) -> Result<String, StoreError> {
        let comment = self.ensure_note_belongs_to_plan(plan, note).await?;
        let decoded = Self::decode_comment_to_note(comment)?;
        Ok(decoded.body)
    }

    async fn write_note_body(
        &self,
        plan: &PlanId,
        note: &NoteId,
        body: &str,
    ) -> Result<(), StoreError> {
        let comment = self.ensure_note_belongs_to_plan(plan, note).await?;
        let decoded = Self::decode_comment_to_note(comment)?;
        let comment_id = Self::parse_note_id(note)?;

        let final_body = note_meta_update_to_comment_body(
            &DecodedNote {
                note: decoded.note.clone(),
                body: body.to_string(),
                client_id: decoded.client_id.clone(),
            },
            &NoteMetaUpdate::default(),
        );

        self.client
            .update_comment(comment_id, UpdateComment { body: final_body })
            .await
            .map_err(map_github_error)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests;

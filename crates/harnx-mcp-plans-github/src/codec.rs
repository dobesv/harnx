//! Codec for mapping between core domain structs and GitHub issue/comment content.
//!
//! This module implements the encoding/decoding of `Plan`, `Task`, and `Note` structs
//! to/from GitHub issue bodies and comments. Metadata is stored in YAML front-matter
//! embedded in the issue body, mirroring the filesystem format from `harnx-mcp-plans`.
//!
//! ## Format
//!
//! Issue body format:
//! ```text
//! ---
//! <yaml front-matter>
//! ---
//! <markdown body>
//! ```
//!
//! ## JIRA Integration
//!
//! When `jira_key` is present in the front-matter, the issue title is prefixed with
//! `[PROJ-123] `<title>`. On decode, this prefix is extracted and stripped.
//!
//! ## Dependencies
//!
//! Task dependencies are serialized as `#<issue_number>` strings in front-matter,
//! allowing GitHub to render them as clickable links to related issues.

use std::sync::LazyLock;

use jiff::Timestamp;
use regex::Regex;
use serde::{Deserialize, Serialize};

use harnx_mcp_plans_core::{
    NewNote, NewPlan, NewTask, Note, NoteMetaUpdate, Plan, PlanMetaUpdate, Task, TaskMetaUpdate,
};

// =============================================================================
// JIRA Key Regex
// =============================================================================

/// Regex to match JIRA key prefix in issue titles.
/// Format: `[PROJ-123] ` where PROJ is uppercase letters/digits followed by a number.
static JIRA_KEY_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[([A-Z][A-Z0-9]*-\d+)\]\s*").expect("JIRA key regex should compile")
});

// =============================================================================
// Front-Matter Structs
// =============================================================================

/// YAML front-matter for Plan issues.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct PlanFrontMatter {
    /// Client-provided ID (optional). GitHub issue number is the authoritative ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// JIRA key for cross-reference (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jira_key: Option<String>,
    /// Plan summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Author.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Assignee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// Executor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,
    /// Git branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// GitHub owner/repo for cross-repo references.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_owner_repo: Option<String>,
    /// Creation timestamp (RFC3339).
    pub created_at: String,
    /// Last update timestamp (RFC3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// YAML front-matter for Task issues.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct TaskFrontMatter {
    /// Client-provided ID (optional). GitHub issue number is the authoritative ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// JIRA key for cross-reference (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jira_key: Option<String>,
    /// Task summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Author.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Assignee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// Executor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,
    /// Tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Task status.
    #[serde(default = "default_status")]
    pub status: String,
    /// Dependencies as `#<issue_number>` strings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
    /// Creation timestamp (RFC3339).
    pub created_at: String,
    /// Last update timestamp (RFC3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// YAML front-matter for Note comments.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct NoteFrontMatter {
    /// Client-provided ID (optional). GitHub comment ID is authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Note summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Author.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Creation timestamp (RFC3339).
    pub created_at: String,
    /// Last update timestamp (RFC3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

fn default_status() -> String {
    "open".to_string()
}

// =============================================================================
// Decode Results (with metadata not in core domain structs)
// =============================================================================

/// Result of decoding a Plan from a GitHub issue.
///
/// Contains the core `Plan` struct plus codec-level metadata like `jira_key`
/// and `client_id` that are not part of the core domain model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedPlan {
    /// The core domain Plan struct.
    pub plan: Plan,
    /// The markdown body content.
    pub body: String,
    /// JIRA key extracted from title prefix or front-matter.
    pub jira_key: Option<String>,
    /// Client-provided ID from front-matter (optional).
    pub client_id: Option<String>,
}

/// Result of decoding a Task from a GitHub issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedTask {
    /// The core domain Task struct.
    pub task: Task,
    /// The markdown body content.
    pub body: String,
    /// JIRA key extracted from title prefix or front-matter.
    pub jira_key: Option<String>,
    /// Client-provided ID from front-matter (optional).
    pub client_id: Option<String>,
}

/// Result of decoding a Note from a GitHub comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedNote {
    /// The core domain Note struct.
    pub note: Note,
    /// The markdown body content.
    pub body: String,
    /// Client-provided ID from front-matter (optional).
    pub client_id: Option<String>,
}

// =============================================================================
// Plan Encode/Decode
// =============================================================================

/// Title used when neither a title nor a client id is available.
const UNTITLED_ISSUE_TITLE: &str = "untitled plan";

/// Trim a value and discard it when nothing is left.
fn non_blank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// Pick a non-blank issue title.
///
/// GitHub rejects issues with a blank title (`422 "title can't be blank"`), so an
/// untitled plan or task falls back to its client id and then to a placeholder.
fn issue_title_or_fallback(title: Option<&str>, client_id: &str) -> String {
    non_blank(title)
        .or_else(|| non_blank(Some(client_id)))
        .unwrap_or(UNTITLED_ISSUE_TITLE)
        .to_string()
}

/// Build the title field of an issue update, prefixing the JIRA key when present.
///
/// A blank requested title yields `None` so the existing issue title is left alone
/// rather than PATCHed to something GitHub would reject.
fn issue_title_update(requested: Option<&str>, jira_key: Option<&str>) -> Option<String> {
    non_blank(requested).map(|title| match jira_key {
        Some(key) => format!("[{}] {}", key, title),
        None => title.to_string(),
    })
}

/// Encode a Plan to GitHub issue title and body.
///
/// # Arguments
/// * `plan` - The Plan to encode.
/// * `jira_key` - Optional JIRA key to prefix in the title.
/// * `body` - The markdown body content.
///
/// # Returns
/// (issue_title, issue_body) tuple.
pub fn plan_to_issue(plan: &Plan, jira_key: Option<&str>, body: &str) -> (String, String) {
    // Build title: prefix with JIRA key if present
    let issue_title = issue_title_or_fallback(plan.title.as_deref(), &plan.id);
    let title = match jira_key {
        Some(key) => format!("[{}] {}", key, issue_title),
        None => issue_title,
    };

    // Build front-matter
    let front = PlanFrontMatter {
        client_id: if plan.id.is_empty() {
            None
        } else {
            Some(plan.id.clone())
        },
        jira_key: jira_key.map(|s| s.to_string()),
        summary: plan.summary.clone(),
        author: plan.author.clone(),
        assignee: plan.assignee.clone(),
        executor: plan.executor.clone(),
        git_branch: plan.git_branch.clone(),
        github_owner_repo: plan.github_owner_repo.clone(),
        created_at: timestamp_to_rfc3339(&plan.created_at),
        updated_at: plan.updated_at.as_ref().map(timestamp_to_rfc3339),
    };

    let issue_body = serialize_frontmatter(&front, body);
    (title, issue_body)
}

/// Decode a GitHub issue to a Plan.
///
/// # Arguments
/// * `issue_number` - The GitHub issue number (authoritative ID).
/// * `title` - The issue title (may include JIRA prefix).
/// * `body` - The issue body (may contain YAML front-matter).
/// * `created_at` - GitHub creation timestamp.
/// * `updated_at` - GitHub update timestamp (optional).
///
/// # Returns
/// DecodedPlan with Plan struct, body, jira_key, and client_id.
pub fn issue_to_plan(
    issue_number: u64,
    title: &str,
    body: Option<&str>,
    created_at: Timestamp,
    updated_at: Option<Timestamp>,
) -> DecodedPlan {
    let body = body.unwrap_or("");
    let (front, markdown_body) = parse_plan_frontmatter(body);

    // Extract JIRA key from title if present
    let (plan_title, jira_key_from_title) = extract_jira_key_from_title(title);

    // Merge JIRA keys: prefer title prefix over front-matter
    let jira_key = jira_key_from_title.or(front.jira_key);

    // Create the Plan domain struct
    // ID is the stringified issue number (authoritative)
    // Note: front.client_id is preserved but not used as the domain ID
    let plan = Plan {
        id: issue_number.to_string(),
        title: Some(plan_title),
        summary: front.summary,
        author: front.author,
        assignee: front.assignee,
        executor: front.executor,
        git_branch: front.git_branch,
        github_owner_repo: front.github_owner_repo,
        created_at,
        updated_at,
    };

    DecodedPlan {
        plan,
        body: markdown_body,
        jira_key,
        client_id: front.client_id,
    }
}

// =============================================================================
// Task Encode/Decode
// =============================================================================

/// Encode a Task to GitHub issue title and body.
///
/// # Arguments
/// * `task` - The Task to encode.
/// * `jira_key` - Optional JIRA key to prefix in the title.
/// * `body` - The markdown body content.
///
/// # Returns
/// (issue_title, issue_body) tuple.
pub fn task_to_issue(task: &Task, jira_key: Option<&str>, body: &str) -> (String, String) {
    // Build title: prefix with JIRA key if present
    let issue_title = issue_title_or_fallback(Some(task.title.as_str()), &task.id);
    let title = match jira_key {
        Some(key) => format!("[{}] {}", key, issue_title),
        None => issue_title,
    };

    // Build dependencies as `#<n>` strings
    let dependencies: Vec<String> = task
        .dependencies
        .iter()
        .map(|dep| {
            // If dependency is already a `#<n>` format, use as-is
            // Otherwise, assume it's an issue number and format as `#<n>`
            if dep.starts_with('#') {
                dep.clone()
            } else {
                format!("#{}", dep)
            }
        })
        .collect();

    // Build front-matter
    let front = TaskFrontMatter {
        client_id: if task.id.is_empty() {
            None
        } else {
            Some(task.id.clone())
        },
        jira_key: jira_key.map(|s| s.to_string()),
        summary: task.summary.clone(),
        author: task.author.clone(),
        assignee: task.assignee.clone(),
        executor: task.executor.clone(),
        tags: task.tags.clone(),
        status: task.status.clone(),
        dependencies,
        created_at: timestamp_to_rfc3339(&task.created_at),
        updated_at: task.updated_at.as_ref().map(timestamp_to_rfc3339),
    };

    let issue_body = serialize_frontmatter(&front, body);
    (title, issue_body)
}

/// Decode a GitHub issue to a Task.
///
/// # Arguments
/// * `plan_id` - The parent Plan ID (issue number of the plan issue).
/// * `issue_number` - The GitHub issue number (authoritative Task ID).
/// * `title` - The issue title (may include JIRA prefix).
/// * `body` - The issue body (may contain YAML front-matter).
/// * `created_at` - GitHub creation timestamp.
/// * `updated_at` - GitHub update timestamp (optional).
///
/// # Returns
/// DecodedTask with Task struct, body, jira_key, and client_id.
pub fn issue_to_task(
    plan_id: String,
    issue_number: u64,
    title: &str,
    body: Option<&str>,
    created_at: Timestamp,
    updated_at: Option<Timestamp>,
) -> DecodedTask {
    let body = body.unwrap_or("");
    let (front, markdown_body) = parse_task_frontmatter(body);

    // Extract JIRA key from title if present
    let (task_title, jira_key_from_title) = extract_jira_key_from_title(title);

    // Merge JIRA keys: prefer title prefix over front-matter
    let jira_key = jira_key_from_title.or(front.jira_key);

    // Parse dependencies: strip `#` prefix to get issue numbers
    let dependencies: Vec<String> = front
        .dependencies
        .iter()
        .filter_map(|dep| dep.strip_prefix('#').map(|s| s.to_string()))
        .collect();

    // Create the Task domain struct
    let task = Task {
        id: issue_number.to_string(),
        title: task_title,
        summary: front.summary,
        author: front.author,
        assignee: front.assignee,
        executor: front.executor,
        tags: front.tags,
        plan: plan_id,
        status: front.status,
        created_at,
        updated_at,
        dependencies,
    };

    DecodedTask {
        task,
        body: markdown_body,
        jira_key,
        client_id: front.client_id,
    }
}

// =============================================================================
// Note Encode/Decode
// =============================================================================

/// Encode a Note to GitHub comment body.
///
/// # Arguments
/// * `note` - The Note to encode.
/// * `body` - The markdown body content.
///
/// # Returns
/// Comment body string.
pub fn note_to_comment(note: &Note, body: &str) -> String {
    // Build front-matter
    let front = NoteFrontMatter {
        client_id: if note.id.is_empty() {
            None
        } else {
            Some(note.id.clone())
        },
        summary: note.summary.clone(),
        author: note.author.clone(),
        created_at: timestamp_to_rfc3339(&note.created_at),
        updated_at: note.updated_at.as_ref().map(timestamp_to_rfc3339),
    };

    serialize_note_frontmatter(&front, body)
}

/// Decode a GitHub comment to a Note.
///
/// # Arguments
/// * `comment_id` - The GitHub comment ID (authoritative Note ID).
/// * `body` - The comment body (may contain YAML front-matter).
/// * `created_at` - GitHub creation timestamp.
/// * `updated_at` - GitHub update timestamp (optional).
///
/// # Returns
/// DecodedNote with Note struct, body, and client_id.
pub fn comment_to_note(
    comment_id: u64,
    body: Option<&str>,
    created_at: Timestamp,
    updated_at: Option<Timestamp>,
) -> DecodedNote {
    let body = body.unwrap_or("");
    let (front, markdown_body) = parse_note_frontmatter(body);

    // Create the Note domain struct
    let note = Note {
        id: comment_id.to_string(),
        summary: front.summary,
        author: front.author,
        created_at,
        updated_at,
    };

    DecodedNote {
        note,
        body: markdown_body,
        client_id: front.client_id,
    }
}

// =============================================================================
// NewPlan/NewTask/NewNote Encode helpers
// =============================================================================

/// Encode a NewPlan to GitHub issue title and body for creation.
///
/// Use this when creating a new plan issue where we don't yet have an issue number.
pub fn new_plan_to_issue(
    new_plan: &NewPlan,
    jira_key: Option<&str>,
    body: &str,
) -> (String, String) {
    let plan = Plan {
        id: new_plan.id.clone(),
        title: new_plan.title.clone(),
        summary: new_plan.summary.clone(),
        author: new_plan.author.clone(),
        assignee: new_plan.assignee.clone(),
        executor: new_plan.executor.clone(),
        git_branch: new_plan.git_branch.clone(),
        github_owner_repo: new_plan.github_owner_repo.clone(),
        created_at: Timestamp::now(),
        updated_at: None,
    };
    plan_to_issue(&plan, jira_key, body)
}

/// Encode a NewTask to GitHub issue title and body for creation.
pub fn new_task_to_issue(
    plan_id: &str,
    new_task: &NewTask,
    jira_key: Option<&str>,
    body: &str,
) -> (String, String) {
    let task = Task {
        id: new_task.id.clone(),
        title: new_task.title.clone(),
        summary: new_task.summary.clone(),
        author: new_task.author.clone(),
        assignee: new_task.assignee.clone(),
        executor: new_task.executor.clone(),
        tags: new_task.tags.clone(),
        plan: plan_id.to_string(),
        status: new_task.status.clone().unwrap_or_else(default_status),
        created_at: Timestamp::now(),
        updated_at: None,
        dependencies: new_task.dependencies.clone(),
    };
    task_to_issue(&task, jira_key, body)
}

/// Encode a NewNote to GitHub comment body for creation.
pub fn new_note_to_comment(new_note: &NewNote, body: &str) -> String {
    let note = Note {
        id: new_note.id.clone(),
        summary: new_note.summary.clone(),
        author: new_note.author.clone(),
        created_at: Timestamp::now(),
        updated_at: None,
    };
    note_to_comment(&note, body)
}

// =============================================================================
// PlanMetaUpdate/TaskMetaUpdate/NoteMetaUpdate Encode helpers
// =============================================================================

/// Build updated issue body for a plan meta update.
///
/// Takes the existing decoded plan and applies the update to produce new front-matter.
pub fn plan_meta_update_to_issue_body(
    existing: &DecodedPlan,
    update: &PlanMetaUpdate,
) -> (Option<String>, String) {
    // Merge update into existing front-matter
    let front = PlanFrontMatter {
        client_id: existing.client_id.clone(),
        jira_key: existing.jira_key.clone(),
        summary: update.summary.clone().or(existing.plan.summary.clone()),
        author: update.author.clone().or(existing.plan.author.clone()),
        assignee: update.assignee.clone().or(existing.plan.assignee.clone()),
        executor: update.executor.clone().or(existing.plan.executor.clone()),
        git_branch: update
            .git_branch
            .clone()
            .or(existing.plan.git_branch.clone()),
        github_owner_repo: update
            .github_owner_repo
            .clone()
            .or(existing.plan.github_owner_repo.clone()),
        created_at: timestamp_to_rfc3339(&existing.plan.created_at),
        updated_at: Some(timestamp_to_rfc3339(&Timestamp::now())),
    };

    let body = serialize_frontmatter(&front, &existing.body);

    // Title update: if specified, needs to include JIRA prefix if present
    let title = issue_title_update(update.title.as_deref(), existing.jira_key.as_deref());

    (title, body)
}

/// Build updated issue body for a task meta update.
pub fn task_meta_update_to_issue_body(
    existing: &DecodedTask,
    update: &TaskMetaUpdate,
) -> (Option<String>, String) {
    // Merge update into existing front-matter
    let dependencies: Vec<String> = update
        .dependencies
        .as_ref()
        .map(|deps| {
            deps.iter()
                .map(|dep| {
                    if dep.starts_with('#') {
                        dep.clone()
                    } else {
                        format!("#{}", dep)
                    }
                })
                .collect()
        })
        .unwrap_or_else(|| {
            existing
                .task
                .dependencies
                .iter()
                .map(|d| format!("#{}", d))
                .collect()
        });

    let front = TaskFrontMatter {
        client_id: existing.client_id.clone(),
        jira_key: existing.jira_key.clone(),
        summary: update.summary.clone().or(existing.task.summary.clone()),
        author: update.author.clone().or(existing.task.author.clone()),
        assignee: update.assignee.clone().or(existing.task.assignee.clone()),
        executor: update.executor.clone().or(existing.task.executor.clone()),
        tags: update
            .tags
            .clone()
            .unwrap_or_else(|| existing.task.tags.clone()),
        status: update
            .status
            .clone()
            .unwrap_or_else(|| existing.task.status.clone()),
        dependencies,
        created_at: timestamp_to_rfc3339(&existing.task.created_at),
        updated_at: Some(timestamp_to_rfc3339(&Timestamp::now())),
    };

    let body = serialize_frontmatter(&front, &existing.body);

    // Title update: if specified, needs to include JIRA prefix if present
    let title = issue_title_update(update.title.as_deref(), existing.jira_key.as_deref());

    (title, body)
}

/// Build updated comment body for a note meta update.
pub fn note_meta_update_to_comment_body(existing: &DecodedNote, update: &NoteMetaUpdate) -> String {
    let front = NoteFrontMatter {
        client_id: existing.client_id.clone(),
        summary: update.summary.clone().or(existing.note.summary.clone()),
        author: update.author.clone().or(existing.note.author.clone()),
        created_at: timestamp_to_rfc3339(&existing.note.created_at),
        updated_at: Some(timestamp_to_rfc3339(&Timestamp::now())),
    };

    serialize_note_frontmatter(&front, &existing.body)
}

// =============================================================================
// Serialization Helpers
// =============================================================================

/// Serialize front-matter and body to issue body string.
fn serialize_frontmatter<T: Serialize>(front: &T, body: &str) -> String {
    let yaml = serde_yaml::to_string(front).expect("YAML serialization should succeed");
    format!("---\n{}---\n{}", yaml, body)
}

/// Serialize note front-matter and body.
fn serialize_note_frontmatter(front: &NoteFrontMatter, body: &str) -> String {
    serialize_frontmatter(front, body)
}

// =============================================================================
// Parsing Helpers
// =============================================================================

/// Parse YAML front-matter from issue body.
/// Returns (front, markdown_body) or default front if missing/malformed.
fn parse_frontmatter<T: Default + for<'de> Deserialize<'de>>(body: &str) -> (T, String) {
    let Some(rest) = body.strip_prefix("---\n") else {
        // No front-matter, return default with whole body
        return (T::default(), body.to_string());
    };

    let Some((front_str, markdown_body)) = rest.split_once("\n---\n") else {
        // Missing terminator, return default
        return (T::default(), body.to_string());
    };

    // Parse YAML
    match serde_yaml::from_str(front_str) {
        Ok(front) => (front, markdown_body.to_string()),
        Err(_) => {
            // Malformed YAML, return default with whole body
            (T::default(), body.to_string())
        }
    }
}

/// Parse Plan front-matter from issue body.
fn parse_plan_frontmatter(body: &str) -> (PlanFrontMatter, String) {
    parse_frontmatter(body)
}

/// Parse Task front-matter from issue body.
fn parse_task_frontmatter(body: &str) -> (TaskFrontMatter, String) {
    parse_frontmatter(body)
}

/// Parse Note front-matter from comment body.
fn parse_note_frontmatter(body: &str) -> (NoteFrontMatter, String) {
    parse_frontmatter(body)
}

/// Extract JIRA key from issue title.
/// Returns (title_without_prefix, Some(jira_key)) or (original_title, None).
fn extract_jira_key_from_title(title: &str) -> (String, Option<String>) {
    match JIRA_KEY_REGEX.captures(title) {
        Some(caps) => {
            let jira_key = caps.get(1).expect("capture group 1").as_str().to_string();
            let title = JIRA_KEY_REGEX.replace(title, "").to_string();
            (title, Some(jira_key))
        }
        None => (title.to_string(), None),
    }
}

// =============================================================================
// Timestamp Helpers
// =============================================================================

/// Convert jiff::Timestamp to RFC3339 string.
fn timestamp_to_rfc3339(ts: &Timestamp) -> String {
    ts.to_string()
}

/// Parse RFC3339 string to jiff::Timestamp.
#[allow(dead_code)]
fn rfc3339_to_timestamp(s: &str) -> Result<Timestamp, jiff::Error> {
    s.parse()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        Timestamp::now()
    }

    #[test]
    fn plan_round_trip() {
        let created = now();
        let plan = Plan {
            id: "123".to_string(),
            title: Some("Test Plan".to_string()),
            summary: Some("A test plan".to_string()),
            author: Some("hestia".to_string()),
            assignee: Some("atlas".to_string()),
            executor: None,
            git_branch: Some("feature/test".to_string()),
            github_owner_repo: Some("owner/repo".to_string()),
            created_at: created,
            updated_at: Some(created),
        };

        let (title, body) = plan_to_issue(&plan, None, "Plan body content");

        let updated = now();
        let decoded = issue_to_plan(123, &title, Some(&body), created, Some(updated));

        assert_eq!(decoded.plan.id, "123");
        assert_eq!(decoded.plan.title, Some("Test Plan".to_string()));
        assert_eq!(decoded.plan.summary, Some("A test plan".to_string()));
        assert_eq!(decoded.plan.author, Some("hestia".to_string()));
        assert_eq!(decoded.plan.assignee, Some("atlas".to_string()));
        assert_eq!(decoded.plan.git_branch, Some("feature/test".to_string()));
        assert_eq!(
            decoded.plan.github_owner_repo,
            Some("owner/repo".to_string())
        );
        assert_eq!(decoded.body, "Plan body content");
        assert_eq!(decoded.jira_key, None);
    }

    #[test]
    fn plan_with_jira_key() {
        let created = now();
        let plan = Plan {
            id: "123".to_string(),
            title: Some("Test Plan".to_string()),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            created_at: created,
            updated_at: None,
        };

        let (title, body) = plan_to_issue(&plan, Some("PROJ-456"), "body");

        assert!(title.starts_with("[PROJ-456]"));
        assert!(title.contains("Test Plan"));

        let decoded = issue_to_plan(123, &title, Some(&body), created, None);

        assert_eq!(decoded.jira_key, Some("PROJ-456".to_string()));
        assert_eq!(decoded.plan.title, Some("Test Plan".to_string()));
    }

    #[test]
    fn task_round_trip() {
        let created = now();
        let task = Task {
            id: "456".to_string(),
            title: "Test Task".to_string(),
            summary: Some("A test task".to_string()),
            author: Some("hestia".to_string()),
            assignee: None,
            executor: None,
            tags: vec!["alpha".to_string(), "beta".to_string()],
            plan: "123".to_string(),
            status: "in_progress".to_string(),
            created_at: created,
            updated_at: Some(created),
            dependencies: vec!["100".to_string(), "200".to_string()],
        };

        let (title, body) = task_to_issue(&task, None, "Task body");

        let updated = now();
        let decoded = issue_to_task(
            "123".to_string(),
            456,
            &title,
            Some(&body),
            created,
            Some(updated),
        );

        assert_eq!(decoded.task.id, "456");
        assert_eq!(decoded.task.title, "Test Task");
        assert_eq!(decoded.task.summary, Some("A test task".to_string()));
        assert_eq!(decoded.task.tags, vec!["alpha", "beta"]);
        assert_eq!(decoded.task.status, "in_progress");
        assert_eq!(decoded.task.dependencies, vec!["100", "200"]);
        assert_eq!(decoded.body, "Task body");
    }

    #[test]
    fn dependencies_as_hash_links() {
        let task = Task {
            id: "1".to_string(),
            title: "Task".to_string(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: vec![],
            plan: "0".to_string(),
            status: "open".to_string(),
            created_at: now(),
            updated_at: None,
            dependencies: vec!["100".to_string(), "200".to_string()],
        };

        let (_, body) = task_to_issue(&task, None, "");

        // Dependencies should be serialized as #100, #200
        assert!(body.contains("#100"));
        assert!(body.contains("#200"));
    }

    #[test]
    fn note_round_trip() {
        let created = now();
        let note = Note {
            id: "789".to_string(),
            summary: Some("Test note".to_string()),
            author: Some("hestia".to_string()),
            created_at: created,
            updated_at: Some(created),
        };

        let body = note_to_comment(&note, "Note content");

        let updated = now();
        let decoded = comment_to_note(789, Some(&body), created, Some(updated));

        assert_eq!(decoded.note.id, "789");
        assert_eq!(decoded.note.summary, Some("Test note".to_string()));
        assert_eq!(decoded.note.author, Some("hestia".to_string()));
        assert_eq!(decoded.body, "Note content");
    }

    #[test]
    fn missing_front_matter() {
        let body = "Just markdown content\nNo front matter here.";

        let decoded = issue_to_plan(1, "Title", Some(body), now(), None);

        assert_eq!(decoded.plan.id, "1");
        assert_eq!(decoded.plan.title, Some("Title".to_string()));
        assert_eq!(decoded.body, body);
    }

    #[test]
    fn malformed_front_matter() {
        let body = "---\nthis is not: valid:: yaml::\n---\nBody content";

        let decoded = issue_to_plan(1, "Title", Some(body), now(), None);

        // Should gracefully fall back to defaults
        assert_eq!(decoded.plan.id, "1");
        assert_eq!(decoded.plan.title, Some("Title".to_string()));
        // Body might be whole content or just "Body content" depending on parse behavior
    }

    #[test]
    fn jira_key_extraction() {
        let (title, jira) = extract_jira_key_from_title("[PROJ-123] Test Title");
        assert_eq!(title, "Test Title");
        assert_eq!(jira, Some("PROJ-123".to_string()));

        let (title2, jira2) = extract_jira_key_from_title("No JIRA prefix");
        assert_eq!(title2, "No JIRA prefix");
        assert_eq!(jira2, None);

        // Edge cases
        let (title3, _jira3) = extract_jira_key_from_title("[ABC-1]");
        assert_eq!(title3, "");

        let (title4, jira4) = extract_jira_key_from_title("[ABC-1]   Spaced Title");
        assert_eq!(title4, "Spaced Title");
        assert_eq!(jira4, Some("ABC-1".to_string()));
    }

    #[test]
    fn client_id_round_trip() {
        let created = now();
        let plan = Plan {
            id: "my-custom-id".to_string(),
            title: Some("Plan".to_string()),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            created_at: created,
            updated_at: None,
        };

        let (_, body) = plan_to_issue(&plan, None, "");

        // After creating, the issue number (say 42) becomes the authoritative ID
        let decoded = issue_to_plan(42, "Plan", Some(&body), created, None);

        // ID should be the issue number
        assert_eq!(decoded.plan.id, "42");
        // But client_id should preserve the original
        assert_eq!(decoded.client_id, Some("my-custom-id".to_string()));
    }

    fn plan_with_title(id: &str, title: Option<&str>) -> Plan {
        Plan {
            id: id.to_string(),
            title: title.map(str::to_string),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            created_at: now(),
            updated_at: None,
        }
    }

    #[test]
    fn untitled_plan_falls_back_to_client_id_for_issue_title() {
        let (title, _) = plan_to_issue(&plan_with_title("my-plan-slug", None), None, "");
        assert_eq!(title, "my-plan-slug");

        let (blank, _) = plan_to_issue(&plan_with_title("my-plan-slug", Some("   ")), None, "");
        assert_eq!(blank, "my-plan-slug");
    }

    #[test]
    fn untitled_plan_with_jira_key_still_has_non_blank_title() {
        let (title, _) = plan_to_issue(&plan_with_title("my-plan-slug", None), Some("ABC-1"), "");
        assert_eq!(title, "[ABC-1] my-plan-slug");
    }

    #[test]
    fn plan_without_title_or_id_falls_back_to_placeholder() {
        let (title, _) = plan_to_issue(&plan_with_title("", None), None, "");
        assert!(!title.trim().is_empty(), "issue title must never be blank");
    }

    #[test]
    fn untitled_task_falls_back_to_client_id_for_issue_title() {
        let task = Task {
            id: "task-slug".to_string(),
            title: String::new(),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            tags: Vec::new(),
            plan: "1".to_string(),
            status: "open".to_string(),
            created_at: now(),
            updated_at: None,
            dependencies: Vec::new(),
        };

        let (title, _) = task_to_issue(&task, None, "");
        assert_eq!(title, "task-slug");
    }

    #[test]
    fn blank_title_updates_leave_issue_titles_unchanged() {
        let (plan_title, _) = plan_meta_update_to_issue_body(
            &issue_to_plan(7, "Existing", Some(""), now(), None),
            &PlanMetaUpdate {
                title: Some("  ".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(plan_title, None, "plan title should be left alone");

        let (task_title, _) = task_meta_update_to_issue_body(
            &issue_to_task("1".to_string(), 8, "Existing", Some(""), now(), None),
            &TaskMetaUpdate {
                title: Some(String::new()),
                ..Default::default()
            },
        );
        assert_eq!(task_title, None, "task title should be left alone");
    }

    #[test]
    fn plan_meta_update_preserves_jira_key() {
        let created = now();
        let plan = Plan {
            id: "1".to_string(),
            title: Some("Original".to_string()),
            summary: None,
            author: None,
            assignee: None,
            executor: None,
            git_branch: None,
            github_owner_repo: None,
            created_at: created,
            updated_at: None,
        };
        let (_, body) = plan_to_issue(&plan, Some("PROJ-999"), "body");

        let decoded = issue_to_plan(1, "[PROJ-999] Original", Some(&body), created, None);

        let update = PlanMetaUpdate {
            title: Some("Updated".to_string()),
            ..Default::default()
        };

        let (new_title, new_body) = plan_meta_update_to_issue_body(&decoded, &update);

        assert_eq!(new_title, Some("[PROJ-999] Updated".to_string()));
        assert!(new_body.contains("jira_key: PROJ-999"));
    }
}

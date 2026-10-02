//! NATS KV storage and markdown document models for plans, tasks, and notes.

use anyhow::{bail, Context, Result};
use async_nats::jetstream::{self, kv};
use futures_util::StreamExt;
use harnx_core::cid_url::{CidUrl, PlanItem};
use serde::{Deserialize, Serialize};

/// KV bucket for plan documents.
pub const PLAN_BUCKET: &str = "harnx_plans";
/// Maximum compare-and-set attempts for a conflicting document update.
pub const MAX_CAS_RETRIES: usize = 5;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanFrontMatter {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_owner_repo: Option<String>,
    #[serde(
        default,
        alias = "parent_issue",
        skip_serializing_if = "Option::is_none"
    )]
    pub github_issue: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_task_url: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFrontMatter {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub plan: String,
    #[serde(default = "default_open_status")]
    pub status: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteFrontMatter {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDocument {
    pub front: PlanFrontMatter,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDocument {
    pub front: TaskFrontMatter,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteDocument {
    pub front: NoteFrontMatter,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDocument {
    pub content: String,
    pub revision: u64,
}

pub fn default_open_status() -> String {
    "open".to_string()
}

fn serialize_document<T: Serialize>(front: &T, body: &str) -> Result<String> {
    let yaml = serde_yaml::to_string(front).context("serialize document front matter")?;
    Ok(format!("---\n{yaml}---\n{body}"))
}

fn split_frontmatter(content: &str) -> Result<(&str, &str)> {
    let rest = content
        .strip_prefix("---\n")
        .context("missing YAML front matter")?;
    rest.split_once("\n---\n")
        .context("missing YAML front matter terminator")
}

pub fn serialize_plan(document: &PlanDocument) -> Result<String> {
    serialize_document(&document.front, &document.body)
}

pub fn parse_plan(content: &str) -> Result<PlanDocument> {
    let (front, body) = split_frontmatter(content)?;
    Ok(PlanDocument {
        front: serde_yaml::from_str(front).context("parse plan front matter")?,
        body: body.to_string(),
    })
}

pub fn serialize_task(document: &TaskDocument) -> Result<String> {
    serialize_document(&document.front, &document.body)
}

pub fn parse_task(content: &str) -> Result<TaskDocument> {
    let (front, body) = split_frontmatter(content)?;
    Ok(TaskDocument {
        front: serde_yaml::from_str(front).context("parse task front matter")?,
        body: body.to_string(),
    })
}

pub fn serialize_note(document: &NoteDocument) -> Result<String> {
    serialize_document(&document.front, &document.body)
}

pub fn parse_note(content: &str) -> Result<NoteDocument> {
    let (front, body) = split_frontmatter(content)?;
    Ok(NoteDocument {
        front: serde_yaml::from_str(front).context("parse note front matter")?,
        body: body.to_string(),
    })
}

/// Ensure the plans bucket exists and has at least the requested replica count.
pub async fn ensure_plans_bucket(
    jetstream: &jetstream::Context,
    replicas: usize,
) -> Result<kv::Store> {
    match jetstream.get_key_value(PLAN_BUCKET).await {
        Ok(store) => {
            if let Err(error) = harnx_nats_common::registry::reconcile_bucket_replicas(
                jetstream,
                PLAN_BUCKET,
                replicas,
            )
            .await
            {
                log::warn!("could not reconcile replicas for plans bucket: {error:#}");
            }
            Ok(store)
        }
        Err(error) if error.kind() == jetstream::context::KeyValueErrorKind::GetBucket => {
            let created = jetstream
                .create_key_value(kv::Config {
                    bucket: PLAN_BUCKET.to_string(),
                    storage: jetstream::stream::StorageType::File,
                    num_replicas: replicas,
                    ..Default::default()
                })
                .await;
            match created {
                Ok(store) => Ok(store),
                Err(_) => jetstream
                    .get_key_value(PLAN_BUCKET)
                    .await
                    .context("failed to create or open plans bucket"),
            }
        }
        Err(error) => Err(error).context("failed to get plans bucket"),
    }
}

/// Get an existing plans bucket, or None if it doesn't exist.
pub async fn optional_plans_bucket(jetstream: &jetstream::Context) -> Result<Option<kv::Store>> {
    match jetstream.get_key_value(PLAN_BUCKET).await {
        Ok(store) => Ok(Some(store)),
        Err(error) if error.kind() == jetstream::context::KeyValueErrorKind::GetBucket => Ok(None),
        Err(error) => Err(error).context("failed to get plans bucket"),
    }
}

fn validate_plan_url(url: &CidUrl) -> Result<()> {
    if matches!(url, CidUrl::Plan { .. }) {
        Ok(())
    } else {
        bail!("expected cid:plan URL")
    }
}

/// Read a plan document and its current KV revision.
pub async fn get_document(store: &kv::Store, url: &CidUrl) -> Result<Option<StoredDocument>> {
    validate_plan_url(url)?;
    let key = url.kv_key();
    let Some(entry) = store.entry(&key).await.context("read plan document")? else {
        return Ok(None);
    };
    if !matches!(entry.operation, kv::Operation::Put) {
        return Ok(None);
    }
    let content = String::from_utf8(entry.value.to_vec()).context("plan document is not UTF-8")?;
    Ok(Some(StoredDocument {
        content,
        revision: entry.revision,
    }))
}

/// Create a document only when its URL is unused.
pub async fn create_document(store: &kv::Store, url: &CidUrl, content: &str) -> Result<u64> {
    validate_plan_url(url)?;
    store
        .create(&url.kv_key(), content.as_bytes().to_vec().into())
        .await
        .with_context(|| format!("create plan document {url}"))
}

/// Apply a read-modify-write operation with bounded revision-CAS retries.
pub async fn update_document<F>(
    store: &kv::Store,
    url: &CidUrl,
    mut edit: F,
) -> Result<StoredDocument>
where
    F: FnMut(&str) -> Result<String>,
{
    validate_plan_url(url)?;
    let key = url.kv_key();
    for _ in 0..=MAX_CAS_RETRIES {
        let current = get_document(store, url)
            .await?
            .with_context(|| format!("plan document not found: {url}"))?;
        let content = edit(&current.content)?;
        match store
            .update(&key, content.as_bytes().to_vec().into(), current.revision)
            .await
        {
            Ok(revision) => return Ok(StoredDocument { content, revision }),
            Err(error) if error.kind() == kv::UpdateErrorKind::WrongLastRevision => continue,
            Err(error) => return Err(error).with_context(|| format!("update plan document {url}")),
        }
    }
    bail!("plan document update conflict after {MAX_CAS_RETRIES} retries: {url}")
}

/// Delete one document with bounded revision-CAS retries.
pub async fn delete_document(store: &kv::Store, url: &CidUrl) -> Result<Option<String>> {
    validate_plan_url(url)?;
    delete_key(store, &url.kv_key()).await
}

async fn delete_key(store: &kv::Store, key: &str) -> Result<Option<String>> {
    for _ in 0..=MAX_CAS_RETRIES {
        let Some(entry) = store.entry(key).await.context("read document for delete")? else {
            return Ok(None);
        };
        if !matches!(entry.operation, kv::Operation::Put) {
            return Ok(None);
        }
        let content =
            String::from_utf8(entry.value.to_vec()).context("plan document is not UTF-8")?;
        match store.purge_expect_revision(key, Some(entry.revision)).await {
            Ok(()) => return Ok(Some(content)),
            Err(error) if error.kind() == kv::PurgeErrorKind::WrongLastRevision => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("delete plan key '{key}'"));
            }
        }
    }
    bail!("plan document delete conflict after {MAX_CAS_RETRIES} retries: {key}")
}

async fn attempt_update<F>(
    store: &kv::Store,
    key: &str,
    current: StoredDocument,
    edit: &mut F,
) -> Result<Option<StoredDocument>>
where
    F: FnMut(&str) -> Result<String>,
{
    let content = edit(&current.content)?;
    match store
        .update(key, content.as_bytes().to_vec().into(), current.revision)
        .await
    {
        Ok(revision) => Ok(Some(StoredDocument { content, revision })),
        Err(error) if error.kind() == kv::UpdateErrorKind::WrongLastRevision => Ok(None),
        Err(error) => Err(error).with_context(|| format!("update plan document key '{key}'")),
    }
}

async fn attempt_create<F>(
    store: &kv::Store,
    key: &str,
    initial: &str,
    edit: &mut F,
) -> Result<Option<StoredDocument>>
where
    F: FnMut(&str) -> Result<String>,
{
    let content = edit(initial)?;
    match store.create(key, content.as_bytes().to_vec().into()).await {
        Ok(revision) => Ok(Some(StoredDocument { content, revision })),
        Err(error) if error.kind() == kv::CreateErrorKind::AlreadyExists => Ok(None),
        Err(error) => Err(error).with_context(|| format!("create plan document key '{key}'")),
    }
}

/// Create or update a document with bounded CAS retries.
pub async fn upsert_document<F>(
    store: &kv::Store,
    url: &CidUrl,
    initial: &str,
    mut edit: F,
) -> Result<StoredDocument>
where
    F: FnMut(&str) -> Result<String>,
{
    validate_plan_url(url)?;
    let key = url.kv_key();
    for _ in 0..=MAX_CAS_RETRIES {
        let stored = if let Some(current) = get_document(store, url).await? {
            attempt_update(store, &key, current, &mut edit).await?
        } else {
            attempt_create(store, &key, initial, &mut edit).await?
        };
        if let Some(stored) = stored {
            return Ok(stored);
        }
    }
    bail!("plan document update conflict after {MAX_CAS_RETRIES} retries: {url}")
}

/// List live documents below a key prefix.
pub async fn list_documents(
    store: &kv::Store,
    prefix: &str,
) -> Result<Vec<(String, StoredDocument)>> {
    let mut keys = store.keys().await.context("list plan document keys")?;
    let mut documents = Vec::new();
    while let Some(key) = keys.next().await {
        let key = key.context("read plan document key")?;
        if !key.starts_with(prefix) {
            continue;
        }
        let Some(entry) = store
            .entry(&key)
            .await
            .context("read listed plan document")?
        else {
            continue;
        };
        if !matches!(entry.operation, kv::Operation::Put) {
            continue;
        }
        let content =
            String::from_utf8(entry.value.to_vec()).context("listed plan document is not UTF-8")?;
        documents.push((
            key,
            StoredDocument {
                content,
                revision: entry.revision,
            },
        ));
    }
    documents.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(documents)
}

pub fn plan_prefix(url: &CidUrl) -> Result<String> {
    let CidUrl::Plan {
        session,
        slug,
        item: PlanItem::Index,
    } = url
    else {
        bail!("expected plan index URL")
    };
    Ok(format!("plan/{}/{slug}/", session.owner()))
}

/// Delete all documents belonging to one plan.
pub async fn delete_plan(store: &kv::Store, url: &CidUrl) -> Result<Vec<(String, String)>> {
    let prefix = plan_prefix(url)?;
    let documents = list_documents(store, &prefix).await?;
    let mut deleted = Vec::new();
    for (key, document) in documents {
        let content = delete_key(store, &key).await?.unwrap_or(document.content);
        deleted.push((key, content));
    }
    Ok(deleted)
}

/// Delete all plan KV keys for an owner (prefix: `plan/<owner>/`).
pub async fn delete_plans_prefix(jetstream: &jetstream::Context, owner: &str) -> Result<usize> {
    let Some(store) = optional_plans_bucket(jetstream).await? else {
        return Ok(0);
    };
    let prefix = format!("plan/{owner}/");
    let documents = list_documents(&store, &prefix).await?;
    for (key, _) in &documents {
        delete_key(&store, key).await?;
    }
    Ok(documents.len())
}

/// Rendered markdown for a mutable plan document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPlan {
    pub markdown: String,
    pub max_revision: u64,
}

struct PlanIndexDocuments {
    plan: PlanDocument,
    tasks: Vec<TaskDocument>,
    notes: Vec<NoteDocument>,
    max_revision: u64,
}

/// Render a plan index, task, or note URL as markdown.
pub async fn render(jetstream: &jetstream::Context, url: &CidUrl) -> Result<RenderedPlan> {
    let CidUrl::Plan { slug, item, .. } = url else {
        bail!("expected cid:plan URL")
    };
    let store = optional_plans_bucket(jetstream)
        .await?
        .with_context(|| format!("plan document not found: {url}"))?;
    match item {
        PlanItem::Index => render_plan_index(&store, url, slug).await,
        PlanItem::Task(_) => render_task(&store, url).await,
        PlanItem::Note(id) => render_note(&store, url, id).await,
    }
}

async fn render_plan_index(store: &kv::Store, url: &CidUrl, slug: &str) -> Result<RenderedPlan> {
    let documents = load_plan_index(store, url).await?;
    Ok(RenderedPlan {
        markdown: index_markdown(slug, &documents.plan, &documents.tasks, &documents.notes)?,
        max_revision: documents.max_revision,
    })
}

async fn load_plan_index(store: &kv::Store, url: &CidUrl) -> Result<PlanIndexDocuments> {
    let root = required_document(store, url).await?;
    let prefix = plan_prefix(url)?;
    let documents = list_documents(store, &prefix).await?;
    let task_prefix = format!("{prefix}tasks/");
    let note_prefix = format!("{prefix}notes/");
    let mut tasks = Vec::new();
    let mut notes = Vec::new();
    let mut max_revision = root.revision;
    for (key, stored) in documents {
        max_revision = max_revision.max(stored.revision);
        if key.starts_with(&task_prefix) {
            tasks.push(parse_task(&stored.content)?);
        } else if key.starts_with(&note_prefix) {
            notes.push(parse_note(&stored.content)?);
        }
    }
    tasks.sort_by(|left, right| task_sort_key(left).cmp(&task_sort_key(right)));
    notes.sort_by(|left, right| note_sort_key(left).cmp(&note_sort_key(right)));
    Ok(PlanIndexDocuments {
        plan: parse_plan(&root.content)?,
        tasks,
        notes,
        max_revision,
    })
}

fn task_sort_key(task: &TaskDocument) -> (&str, &str) {
    (&task.front.created_at, &task.front.id)
}

fn note_sort_key(note: &NoteDocument) -> (&str, &str) {
    (&note.front.created_at, &note.front.id)
}

async fn render_task(store: &kv::Store, url: &CidUrl) -> Result<RenderedPlan> {
    let stored = required_document(store, url).await?;
    let document = parse_task(&stored.content)?;
    Ok(RenderedPlan {
        markdown: task_markdown(url, &document)?,
        max_revision: stored.revision,
    })
}

async fn render_note(store: &kv::Store, url: &CidUrl, id: &str) -> Result<RenderedPlan> {
    let stored = required_document(store, url).await?;
    let document = parse_note(&stored.content)?;
    Ok(RenderedPlan {
        markdown: note_markdown(url, id, &document)?,
        max_revision: stored.revision,
    })
}

async fn required_document(store: &kv::Store, url: &CidUrl) -> Result<StoredDocument> {
    get_document(store, url)
        .await?
        .with_context(|| format!("plan document not found: {url}"))
}

fn index_markdown(
    slug: &str,
    plan: &PlanDocument,
    tasks: &[TaskDocument],
    notes: &[NoteDocument],
) -> Result<String> {
    let title = non_empty(plan.front.title.as_deref()).unwrap_or(slug);
    let mut output = format!("# {title}\n\n");
    append_optional_block(&mut output, plan.front.summary.as_deref());
    append_block(&mut output, &plan.body);
    output.push_str("## Tasks\n\n| Status | Task | Dependencies |\n|---|---|---|\n");
    for task in tasks {
        output.push_str(&task_row(task)?);
    }
    output.push_str("\n## Notes\n\n");
    for note in notes {
        output.push_str(&note_list_item(note));
    }
    Ok(output)
}

fn task_row(task: &TaskDocument) -> Result<String> {
    let dependencies = dependency_links(&task.front.dependencies)?;
    let dependencies = if dependencies.is_empty() {
        "-".to_string()
    } else {
        dependencies
    };
    let title = non_empty(Some(task.front.title.trim())).unwrap_or(&task.front.id);
    Ok(format!(
        "| {} | {} | {} |\n",
        table_text(&task.front.status),
        table_text(&markdown_link(title, &task.front.id)),
        table_text(&dependencies)
    ))
}

fn note_list_item(note: &NoteDocument) -> String {
    let id = plan_item_id(&note.front.id).unwrap_or_else(|| note.front.id.clone());
    let label = non_empty(note.front.summary.as_deref()).unwrap_or(&id);
    let author = non_empty(note.front.author.as_deref())
        .map(|value| format!(" — {value}"))
        .unwrap_or_default();
    format!("- {}{author}\n", markdown_link(label, &note.front.id))
}

fn task_markdown(url: &CidUrl, task: &TaskDocument) -> Result<String> {
    let plan = plan_index_url(url)?;
    let mut output = format!(
        "{}\n\n# {}\n\nStatus: `{}`\n\n",
        markdown_link("Plan Index", &plan.to_string()),
        task.front.title,
        task.front.status
    );
    if let Some(summary) = non_empty(task.front.summary.as_deref()) {
        output.push_str(&format!("Summary: {summary}\n\n"));
    }
    let dependencies = dependency_links(&task.front.dependencies)?;
    if !dependencies.is_empty() {
        output.push_str(&format!("Dependencies: {dependencies}\n\n"));
    }
    append_block(&mut output, &task.body);
    Ok(output)
}

fn note_markdown(url: &CidUrl, id: &str, note: &NoteDocument) -> Result<String> {
    let plan = plan_index_url(url)?;
    let label = non_empty(note.front.summary.as_deref()).unwrap_or(id);
    let mut output = format!(
        "{}\n\n# Note: {label}\n\n",
        markdown_link("Plan Index", &plan.to_string())
    );
    if let Some(author) = non_empty(note.front.author.as_deref()) {
        output.push_str(&format!("Author: {author}\n\n"));
    }
    append_block(&mut output, &note.body);
    Ok(output)
}

fn plan_index_url(url: &CidUrl) -> Result<CidUrl> {
    let CidUrl::Plan { session, slug, .. } = url else {
        bail!("expected cid:plan URL")
    };
    Ok(CidUrl::Plan {
        session: session.clone(),
        slug: slug.clone(),
        item: PlanItem::Index,
    })
}

fn dependency_links(dependencies: &[String]) -> Result<String> {
    dependencies
        .iter()
        .map(|dependency| {
            let url = CidUrl::parse(dependency)
                .with_context(|| format!("invalid task dependency URL: {dependency}"))?;
            let CidUrl::Plan {
                item: PlanItem::Task(id),
                ..
            } = url
            else {
                bail!("task dependency is not a task URL: {dependency}")
            };
            Ok(markdown_link(&id, dependency))
        })
        .collect::<Result<Vec<_>>>()
        .map(|links| links.join(", "))
}

fn plan_item_id(url: &str) -> Option<String> {
    match CidUrl::parse(url).ok()? {
        CidUrl::Plan {
            item: PlanItem::Task(id) | PlanItem::Note(id),
            ..
        } => Some(id),
        _ => None,
    }
}

fn markdown_link(label: &str, url: &str) -> String {
    let label = label
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]");
    format!("[{label}]({url})")
}

fn table_text(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', " ")
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn append_optional_block(output: &mut String, value: Option<&str>) {
    if let Some(value) = non_empty(value) {
        append_block(output, value);
    }
}

fn append_block(output: &mut String, value: &str) {
    let value = value.trim_end_matches('\n');
    if !value.is_empty() {
        output.push_str(value);
        output.push_str("\n\n");
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use harnx_core::cid_url::SessionRef;

    fn plan_url(item: PlanItem) -> CidUrl {
        CidUrl::Plan {
            session: SessionRef::new(None, "abcDEF".to_string()).unwrap(),
            slug: "demo-plan".to_string(),
            item,
        }
    }

    fn task(id: &str, title: &str, created_at: &str, dependencies: Vec<String>) -> TaskDocument {
        let url = plan_url(PlanItem::Task(id.to_string()));
        TaskDocument {
            front: TaskFrontMatter {
                id: url.to_string(),
                title: title.to_string(),
                summary: Some(format!("{title} summary")),
                author: None,
                assignee: None,
                executor: None,
                tags: Vec::new(),
                plan: plan_url(PlanItem::Index).to_string(),
                status: "open".to_string(),
                created_at: created_at.to_string(),
                updated_at: None,
                dependencies,
            },
            body: format!("{title} body"),
        }
    }

    #[test]
    fn escapes_task_titles_and_falls_back_to_task_id() {
        let escaped = task(
            "escaped",
            "Title | with\nnewline",
            "2026-01-01T00:00:00Z",
            Vec::new(),
        );
        let plan = PlanDocument {
            front: PlanFrontMatter::default(),
            body: String::new(),
        };
        let markdown = index_markdown("demo-plan", &plan, std::slice::from_ref(&escaped), &[])
            .expect("render index");
        assert!(markdown.contains("[Title \\| with newline]"));

        let escaped_row = task_row(&escaped).expect("render escaped task row");
        assert!(escaped_row.contains("[Title \\| with newline]"));
        assert_eq!(
            count_unescaped_pipes(&escaped_row) - 1,
            3,
            "escaped row has three columns: {escaped_row}"
        );

        let blank = task("blank", "  \n ", "2026-01-02T00:00:00Z", Vec::new());
        let blank_row = task_row(&blank).expect("render blank task row");
        assert!(blank_row.contains(&format!("[{}]({})", blank.front.id, blank.front.id)));
        assert_eq!(
            count_unescaped_pipes(&blank_row) - 1,
            3,
            "blank row has three columns: {blank_row}"
        );
    }

    fn count_unescaped_pipes(row: &str) -> usize {
        row.char_indices()
            .filter(|(index, character)| {
                *character == '|'
                    && row[..*index]
                        .chars()
                        .rev()
                        .take_while(|c| *c == '\\')
                        .count()
                        % 2
                        == 0
            })
            .count()
    }

    #[test]
    fn renders_plan_index_markdown() {
        let first = task("first", "First task", "2026-01-01T00:00:00Z", Vec::new());
        let second_url = plan_url(PlanItem::Task("second".to_string()));
        let mut second = task(
            "second",
            "Second task",
            "2026-01-02T00:00:00Z",
            vec![first.front.id.clone()],
        );
        second.front.status = "closed".to_string();
        let note_url = plan_url(PlanItem::Note("decision".to_string()));
        let note = NoteDocument {
            front: NoteFrontMatter {
                id: note_url.to_string(),
                summary: Some("Architecture decision".to_string()),
                author: Some("Atlas".to_string()),
                created_at: "2026-01-03T00:00:00Z".to_string(),
                updated_at: None,
            },
            body: "Decision body".to_string(),
        };
        let plan = PlanDocument {
            front: PlanFrontMatter {
                id: plan_url(PlanItem::Index).to_string(),
                title: Some("Demo Plan".to_string()),
                summary: Some("Plan summary".to_string()),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                ..PlanFrontMatter::default()
            },
            body: "Plan body".to_string(),
        };

        let markdown = index_markdown("demo-plan", &plan, &[first.clone(), second], &[note])
            .expect("render index");
        let expected = format!(
            "# Demo Plan\n\nPlan summary\n\nPlan body\n\n## Tasks\n\n| Status | Task | Dependencies |\n|---|---|---|\n| open | [First task]({}) | - |\n| closed | [Second task]({second_url}) | [first]({}) |\n\n## Notes\n\n- [Architecture decision]({note_url}) — Atlas\n",
            first.front.id, first.front.id
        );
        assert_eq!(markdown, expected);
    }

    #[test]
    fn renders_task_markdown() {
        let dependency = plan_url(PlanItem::Task("first".to_string()));
        let url = plan_url(PlanItem::Task("second".to_string()));
        let mut document = task(
            "second",
            "Second task",
            "2026-01-02T00:00:00Z",
            vec![dependency.to_string()],
        );
        document.front.status = "in_progress".to_string();
        let markdown = task_markdown(&url, &document).expect("render task");
        assert_eq!(
            markdown,
            format!(
                "[Plan Index]({})\n\n# Second task\n\nStatus: `in_progress`\n\nSummary: Second task summary\n\nDependencies: [first]({dependency})\n\nSecond task body\n\n",
                plan_url(PlanItem::Index)
            )
        );
    }

    #[test]
    fn renders_note_markdown() {
        let url = plan_url(PlanItem::Note("decision".to_string()));
        let document = NoteDocument {
            front: NoteFrontMatter {
                id: url.to_string(),
                summary: Some("Architecture decision".to_string()),
                author: Some("Atlas".to_string()),
                created_at: "2026-01-03T00:00:00Z".to_string(),
                updated_at: None,
            },
            body: "Use JetStream KV.".to_string(),
        };
        let markdown = note_markdown(&url, "decision", &document).expect("render note");
        assert_eq!(
            markdown,
            format!(
                "[Plan Index]({})\n\n# Note: Architecture decision\n\nAuthor: Atlas\n\nUse JetStream KV.\n\n",
                plan_url(PlanItem::Index)
            )
        );
    }
}

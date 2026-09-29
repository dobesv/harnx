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

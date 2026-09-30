use super::super::params::*;
use super::super::store::*;
use super::OperationContext;
use anyhow::anyhow;
use harnx_blob_store::plans::{
    create_document, delete_plan, get_document, list_documents, parse_plan, serialize_plan,
    upsert_document, PlanDocument, PlanFrontMatter, StoredDocument,
};
use harnx_core::cid_url::{CidUrl, PlanItem};
use rmcp::model::{CallToolResult, ErrorData};
use serde_json::{json, Value};

pub(crate) async fn list(
    context: &OperationContext<'_>,
    params: ListPlansParams,
) -> Result<CallToolResult, ErrorData> {
    let prefix = validate_owner(params.owner.as_ref())?;
    let documents = list_documents(context.store, &prefix)
        .await
        .map_err(map_internal)?;
    let mut plans = Vec::new();
    for (_, stored) in documents.iter().filter(|(key, _)| is_plan_root(key)) {
        let document = parse_plan(&stored.content).map_err(map_internal)?;
        let url = parse_plan_url(&document.front.id)?;
        let plan_prefix = harnx_blob_store::plans::plan_prefix(&url).map_err(map_internal)?;
        let task_count = count_children(&documents, &plan_prefix, "tasks");
        let note_count = count_children(&documents, &plan_prefix, "notes");
        plans.push(plan_json(&document, task_count, note_count));
        context.touch(&url).await;
    }
    result_json(Value::Array(plans))
}

fn validate_owner(owner: Option<&String>) -> Result<String, ErrorData> {
    let Some(owner) = owner else {
        return Ok("plan/".to_string());
    };
    let valid = owner.len() == 64
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
    if !valid {
        return Err(ErrorData::invalid_params(
            "owner must be a 64-character lowercase hexadecimal session key",
            None,
        ));
    }
    Ok(format!("plan/{owner}/"))
}

fn is_plan_root(key: &str) -> bool {
    let segments: Vec<&str> = key.split('/').collect();
    segments.len() == 4 && segments[0] == "plan" && segments[3] == "plan"
}

fn count_children(documents: &[(String, StoredDocument)], prefix: &str, sub: &str) -> usize {
    let child_prefix = format!("{prefix}{sub}/");
    documents
        .iter()
        .filter(|(key, _)| key.starts_with(&child_prefix))
        .count()
}

fn plan_json(document: &PlanDocument, task_count: usize, note_count: usize) -> Value {
    json!({
        "id": document.front.id,
        "title": document.front.title,
        "summary": document.front.summary,
        "author": document.front.author,
        "assignee": document.front.assignee,
        "executor": document.front.executor,
        "git_branch": document.front.git_branch,
        "github_owner_repo": document.front.github_owner_repo,
        "created_at": document.front.created_at,
        "updated_at": document.front.updated_at,
        "task_count": task_count,
        "note_count": note_count,
    })
}

pub(crate) async fn add(
    context: &OperationContext<'_>,
    params: AddPlanParams,
) -> Result<CallToolResult, ErrorData> {
    let caller = context.caller.ok_or_else(|| {
        ErrorData::invalid_params("caller session identity required to create plan", None)
    })?;
    let session = session_from_tool(caller)?;
    let base_slug = slugify(&params.name)?;
    let body = add_body(params.body.clone(), params.content.clone())?;
    for suffix in 1..=10_000 {
        let slug = if suffix == 1 {
            base_slug.clone()
        } else {
            format!("{base_slug}-{suffix}")
        };
        let url = CidUrl::Plan {
            session: session.clone(),
            slug,
            item: PlanItem::Index,
        };
        let document = new_document(&url, &params, body.clone());
        let serialized = serialize_plan(&document).map_err(map_internal)?;
        match create_document(context.store, &url, &serialized).await {
            Ok(_) => {
                context.touch(&url).await;
                let path = format!("{url}/plan.md");
                let diff = diff_text(DiffText {
                    before: "",
                    after: &serialized,
                    path: &path,
                });
                let label = params
                    .title
                    .as_deref()
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or(&params.name);
                return result_with_diff_and_resources(
                    DiffResult {
                        message: format!("added plan {url}"),
                        diff,
                    },
                    vec![ResponseResource::new(&url, "plan", label)],
                );
            }
            Err(_)
                if get_document(context.store, &url)
                    .await
                    .map_err(map_internal)?
                    .is_some() => {}
            Err(error) => return Err(map_internal(error)),
        }
    }
    Err(ErrorData::internal_error(
        "could not allocate a unique plan slug",
        None,
    ))
}

fn add_body(body: Option<String>, content: Option<String>) -> Result<String, ErrorData> {
    if body.is_some() && content.is_some() {
        return Err(ErrorData::invalid_params(
            "provide at most one of body, content",
            None,
        ));
    }
    Ok(content.or(body).unwrap_or_default())
}

fn new_document(url: &CidUrl, params: &AddPlanParams, body: String) -> PlanDocument {
    PlanDocument {
        front: PlanFrontMatter {
            id: url.to_string(),
            title: params.title.clone(),
            summary: params.summary.clone(),
            author: params.author.clone(),
            assignee: params.assignee.clone(),
            executor: params.executor.clone(),
            git_branch: params.git_branch.clone(),
            github_owner_repo: params.github_owner_repo.clone(),
            created_at: now_iso(),
            updated_at: None,
        },
        body,
    }
}

pub(crate) async fn get(
    context: &OperationContext<'_>,
    params: GetPlanParams,
) -> Result<CallToolResult, ErrorData> {
    let url = parse_plan_url(&params.plan)?;
    let stored = get_document(context.store, &url)
        .await
        .map_err(map_internal)?
        .ok_or_else(|| ErrorData::invalid_params(format!("plan not found: {url}"), None))?;
    let document = parse_plan(&stored.content).map_err(map_internal)?;
    let prefix = harnx_blob_store::plans::plan_prefix(&url).map_err(map_internal)?;
    let children = list_documents(context.store, &prefix)
        .await
        .map_err(map_internal)?;
    let task_ids = child_ids(&children, &format!("{prefix}tasks/"), |content| {
        harnx_blob_store::plans::parse_task(content)
            .ok()
            .map(|task| task.front.id)
    });
    let note_ids = child_ids(&children, &format!("{prefix}notes/"), |content| {
        harnx_blob_store::plans::parse_note(content)
            .ok()
            .map(|note| note.front.id)
    });
    context.touch(&url).await;
    let label = plan_label(&document, &url);
    let value = json!({
        "id": document.front.id,
        "title": document.front.title,
        "summary": document.front.summary,
        "author": document.front.author,
        "assignee": document.front.assignee,
        "executor": document.front.executor,
        "git_branch": document.front.git_branch,
        "github_owner_repo": document.front.github_owner_repo,
        "created_at": document.front.created_at,
        "updated_at": document.front.updated_at,
        "body": document.body,
        "task_ids": task_ids,
        "note_ids": note_ids,
    });
    result_json_with_resources(value, vec![ResponseResource::new(&url, "plan", label)])
}

fn child_ids<F>(
    documents: &[(String, harnx_blob_store::plans::StoredDocument)],
    path_prefix: &str,
    parse_id: F,
) -> Vec<String>
where
    F: Fn(&str) -> Option<String>,
{
    documents
        .iter()
        .filter(|(key, _)| key.starts_with(path_prefix))
        .filter_map(|(_, document)| parse_id(&document.content))
        .collect()
}

fn plan_label(document: &PlanDocument, url: &CidUrl) -> String {
    non_empty(document.front.title.as_deref())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| plan_slug(url).to_string())
}

pub(crate) async fn update(
    context: &OperationContext<'_>,
    params: UpdatePlanParams,
) -> Result<CallToolResult, ErrorData> {
    validate_update(&params)?;
    let url = parse_plan_url(&params.plan)?;
    let exists = get_document(context.store, &url)
        .await
        .map_err(map_internal)?
        .is_some();
    if !exists {
        let Some(caller) = context.caller else {
            return Err(ErrorData::invalid_params(
                "caller session identity required to create a plan",
                None,
            ));
        };
        let caller_owner =
            harnx_core::session_identity::session_key(caller.agent.as_deref(), &caller.session_id);
        if url.owner() != caller_owner {
            return Err(ErrorData::invalid_params(
                "caller session does not own the plan being created",
                None,
            ));
        }
    }
    if exists && params.parent_issue.is_some() {
        return Err(ErrorData::invalid_params(
            "parent_issue can only be set when creating a plan, not when updating an existing plan",
            None,
        ));
    }
    let initial = serialize_plan(&PlanDocument {
        front: PlanFrontMatter {
            id: url.to_string(),
            created_at: now_iso(),
            ..PlanFrontMatter::default()
        },
        body: String::new(),
    })
    .map_err(map_internal)?;
    let mut before_body = String::new();
    let edit_params = params.clone();
    let stored = upsert_document(context.store, &url, &initial, |content| {
        let document = parse_plan(content)?;
        before_body.clone_from(&document.body);
        update_document(document, &edit_params)
    })
    .await
    .map_err(map_internal)?;
    let updated = parse_plan(&stored.content).map_err(map_internal)?;
    let created = add_batch_tasks(context, &url, params.tasks).await?;
    context.touch(&url).await;
    let message = if created.is_empty() {
        format!("updated plan {url}")
    } else {
        format!("updated plan {url} and added tasks {}", created.join(", "))
    };
    let path = format!("{url}/plan.md");
    let diff = diff_text(DiffText {
        before: &before_body,
        after: &updated.body,
        path: &path,
    });
    let label = plan_label(&updated, &url);
    result_with_diff_and_resources(
        DiffResult { message, diff },
        vec![ResponseResource::new(&url, "plan", label)],
    )
}

fn validate_update(params: &UpdatePlanParams) -> Result<(), ErrorData> {
    let count = [
        params.content.is_some(),
        params.replace_content.is_some(),
        params.append_content.is_some(),
        params.replace_in_content.is_some(),
    ]
    .into_iter()
    .filter(|provided| *provided)
    .count();
    if count > 1 {
        return Err(ErrorData::invalid_params(
            "provide at most one of content, replace_content, append_content, replace_in_content",
            None,
        ));
    }
    Ok(())
}

fn update_document(
    mut document: PlanDocument,
    params: &UpdatePlanParams,
) -> anyhow::Result<String> {
    document.body = apply_body_edit(
        &document.body,
        BodyEdit {
            replace: params
                .content
                .clone()
                .or_else(|| params.replace_content.clone()),
            append: params.append_content.clone(),
            replace_in: params.replace_in_content.clone(),
        },
    )
    .map_err(|error| anyhow!(error.message.to_string()))?;
    document.front.title = params.title.clone().or(document.front.title);
    document.front.summary = params.summary.clone().or(document.front.summary);
    document.front.author = params.author.clone().or(document.front.author);
    document.front.assignee = params.assignee.clone().or(document.front.assignee);
    document.front.executor = params.executor.clone().or(document.front.executor);
    document.front.git_branch = params.git_branch.clone().or(document.front.git_branch);
    document.front.github_owner_repo = params
        .github_owner_repo
        .clone()
        .or(document.front.github_owner_repo);
    document.front.updated_at = Some(now_iso());
    serialize_plan(&document)
}

async fn add_batch_tasks(
    context: &OperationContext<'_>,
    plan: &CidUrl,
    specs: Option<Vec<TaskSpec>>,
) -> Result<Vec<String>, ErrorData> {
    let mut urls = Vec::new();
    for spec in specs.unwrap_or_default() {
        urls.push(super::task::add_spec(context, plan, spec).await?);
    }
    Ok(urls)
}

pub(crate) async fn delete(
    context: &OperationContext<'_>,
    params: DeletePlanParams,
) -> Result<CallToolResult, ErrorData> {
    let url = parse_plan_url(&params.plan)?;
    let deleted = delete_plan(context.store, &url)
        .await
        .map_err(map_internal)?;
    if deleted.is_empty() {
        return Err(ErrorData::invalid_params(
            format!("plan not found: {url}"),
            None,
        ));
    }
    let diffs = deleted
        .iter()
        .map(|(key, content)| {
            diff_text(DiffText {
                before: content,
                after: "",
                path: key,
            })
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    context.touch(&url).await;
    result_with_diff(DiffResult {
        message: format!("deleted plan {url}"),
        diff: diffs,
    })
}

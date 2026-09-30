use super::super::params::*;
use super::super::store::*;
use super::{require_plan, OperationContext};
use anyhow::anyhow;
use harnx_blob_store::plans::{
    create_document, delete_document, get_document, list_documents, parse_task, serialize_task,
    update_document, TaskDocument, TaskFrontMatter,
};
use harnx_core::cid_url::{CidUrl, PlanItem};
use rmcp::model::{CallToolResult, ErrorData};
use serde_json::{json, Value};

pub(crate) async fn list(
    context: &OperationContext<'_>,
    params: ListTasksParams,
) -> Result<CallToolResult, ErrorData> {
    let plan = parse_plan_url(&params.plan)?;
    require_plan(context, &plan).await?;
    let prefix = format!(
        "{}tasks/",
        harnx_blob_store::plans::plan_prefix(&plan).map_err(map_internal)?
    );
    let documents = list_documents(context.store, &prefix)
        .await
        .map_err(map_internal)?;
    let mut tasks = Vec::new();
    for (_, stored) in documents {
        let task = parse_task(&stored.content).map_err(map_internal)?;
        if params.filter != "all" && task.front.status != params.filter {
            continue;
        }
        if params
            .tag
            .as_ref()
            .is_some_and(|tag| !task.front.tags.contains(tag))
        {
            continue;
        }
        tasks.push(task_json(&task));
    }
    context.touch(&plan).await;
    result_json(Value::Array(tasks))
}

pub(crate) async fn get(
    context: &OperationContext<'_>,
    params: GetTaskParams,
) -> Result<CallToolResult, ErrorData> {
    let (plan, url) = task_location(&params.plan, &params.id)?;
    let stored = get_document(context.store, &url)
        .await
        .map_err(map_internal)?
        .ok_or_else(|| ErrorData::invalid_params(format!("task not found: {url}"), None))?;
    let task = parse_task(&stored.content).map_err(map_internal)?;
    context.touch(&plan).await;
    result_json_with_resources(
        task_json(&task),
        item_resources(&plan, &url, "task", &task_label(&task, &url)),
    )
}

fn task_location(plan: &str, id: &str) -> Result<(CidUrl, CidUrl), ErrorData> {
    let plan = parse_plan_url(plan)?;
    let task = parse_task_url(&plan, id)?;
    Ok((plan, task))
}

pub(crate) async fn add(
    context: &OperationContext<'_>,
    params: AddTaskParams,
) -> Result<CallToolResult, ErrorData> {
    let plan = parse_plan_url(&params.plan)?;
    require_plan(context, &plan).await?;
    let spec = TaskSpec {
        title: params.title,
        id: params.id,
        tags: params.tags,
        dependencies: params.dependencies,
        status: params.status,
        body: params.body,
        summary: params.summary,
        author: params.author,
        assignee: params.assignee,
        executor: params.executor,
    };
    let url = add_spec(context, &plan, spec).await?;
    let task_url = CidUrl::parse(&url).map_err(map_internal)?;
    let stored = get_document(context.store, &task_url)
        .await
        .map_err(map_internal)?
        .ok_or_else(|| ErrorData::internal_error("new task was not stored", None))?;
    let task = parse_task(&stored.content).map_err(map_internal)?;
    let path = format!("{url}.md");
    let diff = diff_text(DiffText {
        before: "",
        after: &stored.content,
        path: &path,
    });
    context.touch(&plan).await;
    result_with_diff_and_resources(
        DiffResult {
            message: format!("added task {url} to plan {plan}"),
            diff,
        },
        item_resources(&plan, &task_url, "task", &task_label(&task, &task_url)),
    )
}

pub(super) async fn add_spec(
    context: &OperationContext<'_>,
    plan: &CidUrl,
    spec: TaskSpec,
) -> Result<String, ErrorData> {
    let mut spec = spec;
    spec.dependencies = validate_dependencies(spec.dependencies)?;
    if let Some(raw_id) = spec.id.clone() {
        let id = validate_item_id(&raw_id)?;
        return create_spec(context, plan, spec, id).await;
    }
    for _ in 0..100 {
        let id = gen_id();
        match create_spec(context, plan, spec.clone(), id).await {
            Ok(url) => return Ok(url),
            Err(error) if error.message.contains("already exists") => continue,
            Err(error) => return Err(error),
        }
    }
    Err(ErrorData::internal_error(
        "could not allocate a unique task ID",
        None,
    ))
}

async fn create_spec(
    context: &OperationContext<'_>,
    plan: &CidUrl,
    spec: TaskSpec,
    id: String,
) -> Result<String, ErrorData> {
    let url = item_url(plan, PlanItem::Task(id));
    let document = TaskDocument {
        front: TaskFrontMatter {
            id: url.to_string(),
            title: spec.title,
            summary: spec.summary,
            author: spec.author,
            assignee: spec.assignee,
            executor: spec.executor,
            tags: spec.tags,
            plan: plan.to_string(),
            status: spec.status.unwrap_or_else(|| "open".to_string()),
            created_at: now_iso(),
            updated_at: None,
            dependencies: spec.dependencies,
        },
        body: spec.body.unwrap_or_default(),
    };
    let serialized = serialize_task(&document).map_err(map_internal)?;
    match create_document(context.store, &url, &serialized).await {
        Ok(_) => Ok(url.to_string()),
        Err(_)
            if get_document(context.store, &url)
                .await
                .map_err(map_internal)?
                .is_some() =>
        {
            Err(ErrorData::invalid_params(
                format!("task already exists: {url}"),
                None,
            ))
        }
        Err(error) => Err(map_internal(error)),
    }
}

pub(crate) async fn update(
    context: &OperationContext<'_>,
    params: UpdateTaskParams,
) -> Result<CallToolResult, ErrorData> {
    let plan = parse_plan_url(&params.plan)?;
    let url = parse_task_url(&plan, &params.id)?;
    let dependencies = params
        .dependencies
        .clone()
        .map(validate_dependencies)
        .transpose()?;
    let mut before_body = String::new();
    let edit_params = params.clone();
    let stored = update_document(context.store, &url, |content| {
        let task = parse_task(content)?;
        before_body.clone_from(&task.body);
        update_task_document(task, &edit_params, dependencies.clone())
    })
    .await
    .map_err(map_internal)?;
    let task = parse_task(&stored.content).map_err(map_internal)?;
    let path = format!("{url}.md");
    let diff = diff_text(DiffText {
        before: &before_body,
        after: &task.body,
        path: &path,
    });
    context.touch(&plan).await;
    result_with_diff_and_resources(
        DiffResult {
            message: format!("updated task {url}"),
            diff,
        },
        item_resources(&plan, &url, "task", &task_label(&task, &url)),
    )
}

fn update_task_document(
    mut task: TaskDocument,
    params: &UpdateTaskParams,
    dependencies: Option<Vec<String>>,
) -> anyhow::Result<String> {
    task.front.title = params.title.clone().unwrap_or(task.front.title);
    task.front.summary = params.summary.clone().or(task.front.summary);
    task.front.author = params.author.clone().or(task.front.author);
    task.front.assignee = params.assignee.clone().or(task.front.assignee);
    task.front.executor = params.executor.clone().or(task.front.executor);
    task.front.tags = params.tags.clone().unwrap_or(task.front.tags);
    task.front.status = params.status.clone().unwrap_or(task.front.status);
    task.front.dependencies = dependencies.unwrap_or(task.front.dependencies);
    task.front.updated_at = Some(now_iso());
    task.body = apply_body_edit(
        &task.body,
        BodyEdit {
            replace: params.replace_body.clone(),
            append: params.append_body.clone(),
            replace_in: params.replace_in_body.clone(),
        },
    )
    .map_err(|error| anyhow!(error.message.to_string()))?;
    serialize_task(&task)
}

pub(crate) async fn delete(
    context: &OperationContext<'_>,
    params: DeleteTaskParams,
) -> Result<CallToolResult, ErrorData> {
    let (plan, url) = task_location(&params.plan, &params.id)?;
    let content = delete_document(context.store, &url)
        .await
        .map_err(map_internal)?
        .ok_or_else(|| ErrorData::invalid_params(format!("task not found: {url}"), None))?;
    let path = format!("{url}.md");
    let diff = diff_text(DiffText {
        before: &content,
        after: "",
        path: &path,
    });
    context.touch(&plan).await;
    result_with_diff(DiffResult {
        message: format!("deleted task {url}"),
        diff,
    })
}

fn task_label(task: &TaskDocument, url: &CidUrl) -> String {
    non_empty(Some(task.front.title.trim()))
        .map(ToOwned::to_owned)
        .or_else(|| plan_item_id(&url.to_string()))
        .unwrap_or_else(|| url.to_string())
}

fn task_json(task: &TaskDocument) -> Value {
    json!({
        "id": task.front.id,
        "title": task.front.title,
        "summary": task.front.summary,
        "author": task.front.author,
        "assignee": task.front.assignee,
        "executor": task.front.executor,
        "tags": task.front.tags,
        "plan": task.front.plan,
        "status": task.front.status,
        "created_at": task.front.created_at,
        "updated_at": task.front.updated_at,
        "dependencies": task.front.dependencies,
        "body": task.body,
    })
}

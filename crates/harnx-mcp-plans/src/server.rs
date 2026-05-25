//! Plans MCP server implementation.
//!
//! Stores plans under per-plan directories using YAML front matter + markdown body.
//! Layout: `<data-dir>/<plan>/plan.md`, `<data-dir>/<plan>/tasks/<id>.md`, and
//! `<data-dir>/<plan>/notes/<id>.md`.

use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorData, Implementation, ListToolsResult,
    Meta, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::schemars::{generate::SchemaGenerator, JsonSchema, Schema};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use similar::{ChangeTag, TextDiff};
use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskFrontMatter {
    id: String,
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    executor: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    plan: String,
    #[serde(default = "default_open_status")]
    status: String,
    created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    dependencies: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskRecord {
    #[serde(flatten)]
    front: TaskFrontMatter,
    #[serde(default)]
    body: String,
}

#[derive(Debug, Serialize)]
struct TaskWithBody<'a> {
    #[serde(flatten)]
    front: &'a TaskFrontMatter,
    body: &'a str,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PlanFrontMatter {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    executor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    git_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    github_owner_repo: Option<String>,
    created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlanRecord {
    #[serde(flatten)]
    front: PlanFrontMatter,
    #[serde(default)]
    body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NoteFrontMatter {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NoteRecord {
    #[serde(flatten)]
    front: NoteFrontMatter,
    #[serde(default)]
    body: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct ListTasksParams {
    plan: String,
    #[serde(default = "default_open_status")]
    filter: String,
    #[serde(default)]
    tag: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct GetTaskParams {
    plan: String,
    id: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct AddTaskParams {
    title: String,
    plan: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    executor: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct UpdateTaskParams {
    plan: String,
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    executor: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    replace_body: Option<String>,
    #[serde(default)]
    append_body: Option<String>,
    #[serde(default)]
    replace_in_body: Option<ReplaceInContent>,
    #[serde(default)]
    dependencies: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct ReplaceInContent {
    old_text: String,
    new_text: String,
    #[serde(default)]
    replace_all: Option<bool>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct DeleteTaskParams {
    plan: String,
    id: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct ListPlansParams {}

#[derive(Debug, Default, Clone, Deserialize)]
struct AddPlanParams {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    executor: Option<String>,
    #[serde(default)]
    git_branch: Option<String>,
    #[serde(default)]
    github_owner_repo: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct GetPlanParams {
    name: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct UpdatePlanParams {
    name: String,
    #[serde(default)]
    replace_content: Option<String>,
    #[serde(default)]
    append_content: Option<String>,
    #[serde(default)]
    replace_in_content: Option<ReplaceInContent>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    executor: Option<String>,
    #[serde(default)]
    git_branch: Option<String>,
    #[serde(default)]
    github_owner_repo: Option<String>,
    #[serde(default)]
    tasks: Option<Vec<TaskSpec>>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct DeletePlanParams {
    name: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct ListNotesParams {
    plan: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct AddNoteParams {
    plan: String,
    #[serde(default)]
    id: Option<String>,
    body: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    author: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct GetNoteParams {
    plan: String,
    note_id: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct DeleteNoteParams {
    plan: String,
    note_id: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct UpdateNoteParams {
    plan: String,
    note_id: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    replace_body: Option<String>,
    #[serde(default)]
    append_body: Option<String>,
    #[serde(default)]
    replace_in_body: Option<ReplaceInContent>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct TaskSpec {
    title: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    executor: Option<String>,
}

pub struct PlansServer {
    dir: PathBuf,
}

impl PlansServer {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    async fn handle_list_tasks(
        &self,
        params: ListTasksParams,
    ) -> Result<CallToolResult, ErrorData> {
        let plan_name =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let status_filter = match params.filter.as_str() {
            "all" => None,
            other => Some(other),
        };
        let tasks = list_tasks(
            &self.dir,
            Some(&plan_name),
            params.tag.as_deref(),
            status_filter,
        );
        let tasks_json: Vec<Value> = tasks
            .iter()
            .map(|task| {
                serde_json::to_value(TaskWithBody {
                    front: &task.front,
                    body: &task.body,
                })
                .map_err(|err| ErrorData::internal_error(err.to_string(), None))
            })
            .collect::<Result<_, _>>()?;
        result_json(Value::Array(tasks_json))
    }

    async fn handle_get_task(&self, params: GetTaskParams) -> Result<CallToolResult, ErrorData> {
        let plan_name =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let task = read_task(&self.dir, &plan_name, &params.id)
            .map_err(|e| ErrorData::invalid_params(e, None))?;

        result_json(
            serde_json::to_value(TaskWithBody {
                front: &task.front,
                body: &task.body,
            })
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?,
        )
    }

    async fn handle_add_task(&self, params: AddTaskParams) -> Result<CallToolResult, ErrorData> {
        let plan_name =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let plan_path = plan_dir(&self.dir, &plan_name);
        if !plan_path.exists() {
            std::fs::create_dir_all(&plan_path)
                .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        }

        let id = params
            .id
            .as_deref()
            .map(validate_id)
            .transpose()
            .map_err(|err| ErrorData::invalid_params(err, None))?
            .unwrap_or_else(gen_id);
        if task_file_path(&self.dir, &plan_name, &id).exists() {
            return Err(ErrorData::invalid_params(
                format!("task '{}' already exists in plan '{}'", id, plan_name),
                None,
            ));
        }
        let now = now_iso();
        let task = TaskRecord {
            front: TaskFrontMatter {
                id: id.clone(),
                title: params.title,
                summary: params.summary,
                author: params.author,
                assignee: params.assignee,
                executor: params.executor,
                tags: params.tags,
                plan: plan_name.clone(),
                status: params.status.unwrap_or_else(default_open_status),
                created_at: now,
                updated_at: None,
                dependencies: params.dependencies,
            },
            body: params.body.unwrap_or_default(),
        };
        write_task(&self.dir, &task).map_err(|err| ErrorData::internal_error(err, None))?;
        let serialized =
            serialize_task(&task).map_err(|err| ErrorData::internal_error(err, None))?;
        let diff = diff_text(
            "",
            &serialized,
            &format!("{}/tasks/{}.md", task.front.plan, task.front.id),
        );
        let msg = format!(
            "added task {} to plan {}",
            display_id(&task.front.id),
            task.front.plan
        );
        if diff.is_empty() {
            result_text(msg)
        } else {
            result_text(format!(
                "{msg}

{diff}"
            ))
        }
    }

    async fn handle_update_task(
        &self,
        params: UpdateTaskParams,
    ) -> Result<CallToolResult, ErrorData> {
        let plan_name =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let mut task = read_task(&self.dir, &plan_name, &params.id)
            .map_err(|err| ErrorData::invalid_params(err, None))?;

        if let Some(title) = params.title {
            task.front.title = title;
        }
        if let Some(summary) = params.summary {
            task.front.summary = Some(summary);
        }
        if let Some(author) = params.author {
            task.front.author = Some(author);
        }
        if let Some(assignee) = params.assignee {
            task.front.assignee = Some(assignee);
        }
        if let Some(executor) = params.executor {
            task.front.executor = Some(executor);
        }
        if let Some(tags) = params.tags {
            task.front.tags = tags;
        }
        if let Some(status) = params.status {
            task.front.status = status;
        }

        // Body-edit: at most one of replace_body / append_body / replace_in_body
        let body_count = [
            params.replace_body.is_some(),
            params.append_body.is_some(),
            params.replace_in_body.is_some(),
        ]
        .iter()
        .filter(|&&b| b)
        .count();
        if body_count > 1 {
            return Err(ErrorData::invalid_params(
                "at most one of replace_body, append_body, replace_in_body may be provided",
                None,
            ));
        }
        let before_body = task.body.clone();
        if let Some(rb) = params.replace_body {
            task.body = rb;
        } else if let Some(ab) = params.append_body {
            if !task.body.is_empty() && !task.body.ends_with('\n') {
                task.body.push('\n');
            }
            task.body.push_str(&ab);
        } else if let Some(ri) = params.replace_in_body {
            task.body = apply_replace_in(&task.body, &ri)?;
        }

        if let Some(dependencies) = params.dependencies {
            task.front.dependencies = dependencies;
        }

        task.front.updated_at = Some(now_iso());

        let task_id = task.front.id.clone();
        write_task(&self.dir, &task).map_err(|err| ErrorData::internal_error(err, None))?;
        let diff = diff_text(
            &before_body,
            &task.body,
            &format!("{}/tasks/{}.md", plan_name, task_id),
        );
        let msg = format!("updated task {}", display_id(&task_id));
        if diff.is_empty() {
            result_text(msg)
        } else {
            result_text(format!("{msg}\n\n{diff}"))
        }
    }

    async fn handle_delete_task(
        &self,
        params: DeleteTaskParams,
    ) -> Result<CallToolResult, ErrorData> {
        let plan_name =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let path = task_file_path(&self.dir, &plan_name, &params.id);
        if !path.exists() {
            return Err(ErrorData::invalid_params(
                format!(
                    "task {} not found in plan '{}'",
                    display_id(&params.id),
                    plan_name
                ),
                None,
            ));
        }
        let before_content = std::fs::read_to_string(&path)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        std::fs::remove_file(&path)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        let diff = diff_text(
            &before_content,
            "",
            &format!("{}/tasks/{}.md", plan_name, normalize_id(&params.id)),
        );
        let msg = format!("deleted task {}", display_id(&params.id));
        if diff.is_empty() {
            result_text(msg)
        } else {
            result_text(format!(
                "{msg}

{diff}"
            ))
        }
    }

    async fn handle_list_plans(&self) -> Result<CallToolResult, ErrorData> {
        let mut plans = Vec::new();
        for dir in plan_dirs(&self.dir) {
            let Some(name) = dir.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            let normalized = normalize_plan_name(name);
            let plan_path = plan_file_path(&self.dir, &normalized);
            let record = if plan_path.exists() {
                let content = std::fs::read_to_string(&plan_path)
                    .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
                let (front, body) = parse_plan_frontmatter(&content, &normalized)
                    .map_err(|err| ErrorData::internal_error(err, None))?;
                PlanRecord { front, body }
            } else {
                PlanRecord {
                    front: PlanFrontMatter {
                        id: normalized.clone(),
                        created_at: String::new(),
                        ..Default::default()
                    },
                    body: String::new(),
                }
            };
            let task_ids = list_tasks(&self.dir, Some(&normalized), None, None)
                .into_iter()
                .map(|task| display_id(&task.front.id))
                .collect::<Vec<_>>();
            let note_ids = list_note_ids(&self.dir, &normalized);
            plans.push(json!({
                "id": record.front.id,
                "title": record.front.title,
                "summary": record.front.summary,
                "author": record.front.author,
                "assignee": record.front.assignee,
                "executor": record.front.executor,
                "git_branch": record.front.git_branch,
                "github_owner_repo": record.front.github_owner_repo,
                "created_at": record.front.created_at,
                "updated_at": record.front.updated_at,
                "task_count": task_ids.len(),
                "note_count": note_ids.len(),
            }));
        }
        result_json(Value::Array(plans))
    }

    async fn handle_add_plan(&self, params: AddPlanParams) -> Result<CallToolResult, ErrorData> {
        let name =
            validate_plan_name(&params.name).map_err(|err| ErrorData::invalid_params(err, None))?;
        let dir = plan_dir(&self.dir, &name);
        if dir.exists() {
            return Err(ErrorData::invalid_params(
                format!("plan '{}' already exists", name),
                None,
            ));
        }
        std::fs::create_dir_all(&dir)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;

        let record = PlanRecord {
            front: PlanFrontMatter {
                id: name.clone(),
                title: params.title,
                summary: params.summary,
                author: params.author,
                assignee: params.assignee,
                executor: params.executor,
                git_branch: params.git_branch,
                github_owner_repo: params.github_owner_repo,
                created_at: now_iso(),
                updated_at: None,
            },
            body: params.body.unwrap_or_default(),
        };
        let serialized =
            serialize_plan(&record).map_err(|err| ErrorData::internal_error(err, None))?;
        write_plan_file(&plan_file_path(&self.dir, &name), &serialized)
            .map_err(|err| ErrorData::internal_error(err, None))?;
        let diff = diff_text("", &serialized, &format!("{name}/plan.md"));
        let msg = format!("added plan {}", name);
        if diff.is_empty() {
            result_text(msg)
        } else {
            result_text(format!("{msg}\n\n{diff}"))
        }
    }

    async fn handle_get_plan(&self, params: GetPlanParams) -> Result<CallToolResult, ErrorData> {
        let name =
            validate_plan_name(&params.name).map_err(|err| ErrorData::invalid_params(err, None))?;
        let path = plan_file_path(&self.dir, &name);
        let body = if path.exists() {
            std::fs::read_to_string(&path)
                .map_err(|err| ErrorData::internal_error(err.to_string(), None))?
        } else if plan_dir(&self.dir, &name).exists() {
            String::new()
        } else {
            return Err(ErrorData::invalid_params(
                format!("plan '{}' not found", name),
                None,
            ));
        };
        let (front, content) = parse_plan_frontmatter(&body, &name)
            .map_err(|err| ErrorData::internal_error(err, None))?;
        let task_ids = list_tasks(&self.dir, Some(&name), None, None)
            .into_iter()
            .map(|task| display_id(&task.front.id))
            .collect::<Vec<_>>();
        let note_ids = list_note_ids(&self.dir, &name);
        result_json(json!({
            "id": front.id,
            "title": front.title,
            "summary": front.summary,
            "author": front.author,
            "assignee": front.assignee,
            "executor": front.executor,
            "git_branch": front.git_branch,
            "github_owner_repo": front.github_owner_repo,
            "created_at": front.created_at,
            "updated_at": front.updated_at,
            "body": content,
            "task_ids": task_ids,
            "note_ids": note_ids,
        }))
    }

    async fn handle_update_plan(
        &self,
        params: UpdatePlanParams,
    ) -> Result<CallToolResult, ErrorData> {
        let name =
            validate_plan_name(&params.name).map_err(|err| ErrorData::invalid_params(err, None))?;
        let dir = plan_dir(&self.dir, &name);
        std::fs::create_dir_all(&dir)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;

        let path = plan_file_path(&self.dir, &name);
        let (existing, existing_body) = if path.exists() {
            let content = std::fs::read_to_string(&path)
                .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
            let (front, body) = parse_plan_frontmatter(&content, &name)
                .map_err(|err| ErrorData::internal_error(err, None))?;
            (front, body)
        } else {
            (
                PlanFrontMatter {
                    id: name.clone(),
                    created_at: now_iso(),
                    ..Default::default()
                },
                String::new(),
            )
        };

        // Body-edit: at most one of replace_content / append_content / replace_in_content
        let body_count = [
            params.replace_content.is_some(),
            params.append_content.is_some(),
            params.replace_in_content.is_some(),
        ]
        .iter()
        .filter(|&&b| b)
        .count();
        if body_count > 1 {
            return Err(ErrorData::invalid_params(
                "at most one of replace_content, append_content, replace_in_content may be provided",
                None,
            ));
        }
        let before_body = existing_body.clone();
        let new_body = if let Some(rc) = params.replace_content {
            rc
        } else if let Some(ac) = params.append_content {
            let mut b = existing_body;
            if !b.is_empty() && !b.ends_with('\n') {
                b.push('\n');
            }
            b.push_str(&ac);
            b
        } else if let Some(ri) = params.replace_in_content {
            apply_replace_in(&existing_body, &ri)?
        } else {
            existing_body
        };

        let record = PlanRecord {
            front: PlanFrontMatter {
                id: name.clone(),
                title: params.title.or(existing.title),
                summary: params.summary.or(existing.summary),
                author: params.author.or(existing.author),
                assignee: params.assignee.or(existing.assignee),
                executor: params.executor.or(existing.executor),
                git_branch: params.git_branch.or(existing.git_branch),
                github_owner_repo: params.github_owner_repo.or(existing.github_owner_repo),
                created_at: if existing.created_at.is_empty() {
                    now_iso()
                } else {
                    existing.created_at
                },
                updated_at: Some(now_iso()),
            },
            body: new_body.clone(),
        };

        // Validate task id uniqueness BEFORE writing anything
        let task_specs = params.tasks;
        if let Some(ref tasks) = task_specs {
            let existing_tasks = list_tasks(&self.dir, Some(&name), None, None);
            let mut seen_ids: Vec<String> = existing_tasks
                .iter()
                .map(|t| normalize_id(&t.front.id))
                .collect();
            for spec in tasks {
                if let Some(ref raw_id) = spec.id {
                    let id =
                        validate_id(raw_id).map_err(|err| ErrorData::invalid_params(err, None))?;
                    if seen_ids.iter().any(|existing| existing == &id) {
                        return Err(ErrorData::invalid_params(
                            format!("task '{}' already exists in plan '{}'", id, name),
                            None,
                        ));
                    }
                    seen_ids.push(id);
                }
            }
        }

        // Build task records before writing anything (all validation already done above)
        let mut task_records = Vec::new();
        if let Some(tasks) = task_specs {
            for spec in tasks {
                let id = spec
                    .id
                    .map(|raw| validate_id(&raw))
                    .transpose()
                    .map_err(|err| ErrorData::invalid_params(err, None))?
                    .unwrap_or_else(gen_id);
                task_records.push(TaskRecord {
                    front: TaskFrontMatter {
                        id,
                        title: spec.title,
                        summary: spec.summary,
                        author: spec.author,
                        assignee: spec.assignee,
                        executor: spec.executor,
                        tags: spec.tags,
                        plan: name.clone(),
                        status: spec.status.unwrap_or_else(default_open_status),
                        created_at: now_iso(),
                        updated_at: None,
                        dependencies: spec.dependencies,
                    },
                    body: spec.body.unwrap_or_default(),
                });
            }
        }

        // Write plan.md first, then tasks — plan always reflects its own metadata
        let serialized =
            serialize_plan(&record).map_err(|err| ErrorData::internal_error(err, None))?;
        write_plan_file(&path, &serialized).map_err(|err| ErrorData::internal_error(err, None))?;

        let mut created_task_ids = Vec::new();
        for task in task_records {
            let id = task.front.id.clone();
            write_task(&self.dir, &task).map_err(|err| ErrorData::internal_error(err, None))?;
            created_task_ids.push(display_id(&id));
        }

        let base_msg = if created_task_ids.is_empty() {
            format!("updated plan {}", name)
        } else {
            format!(
                "updated plan {} and added tasks {}",
                name,
                created_task_ids.join(", ")
            )
        };
        let diff = diff_text(&before_body, &new_body, &format!("{name}/plan.md"));
        if diff.is_empty() {
            result_text(base_msg)
        } else {
            result_text(format!("{base_msg}\n\n{diff}"))
        }
    }

    async fn handle_delete_plan(
        &self,
        params: DeletePlanParams,
    ) -> Result<CallToolResult, ErrorData> {
        let name =
            validate_plan_name(&params.name).map_err(|err| ErrorData::invalid_params(err, None))?;
        let dir = plan_dir(&self.dir, &name);
        if !dir.exists() {
            return Err(ErrorData::invalid_params(
                format!("plan '{}' not found", name),
                None,
            ));
        }
        let mut diffs = Vec::new();

        let plan_file = plan_file_path(&self.dir, &name);
        if plan_file.exists() {
            let content = std::fs::read_to_string(&plan_file).unwrap_or_default();
            let d = diff_text(&content, "", &format!("{name}/plan.md"));
            if !d.is_empty() {
                diffs.push(d);
            }
        }

        let tasks_path = tasks_dir(&self.dir, &name);
        if tasks_path.exists() {
            if let Ok(entries) = std::fs::read_dir(&tasks_path) {
                let mut files: Vec<_> = entries
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(OsStr::to_str) == Some("md"))
                    .collect();
                files.sort();
                for f in files {
                    let stem = f.file_stem().and_then(OsStr::to_str).unwrap_or("task");
                    let content = std::fs::read_to_string(&f).unwrap_or_default();
                    let d = diff_text(&content, "", &format!("{name}/tasks/{stem}.md"));
                    if !d.is_empty() {
                        diffs.push(d);
                    }
                }
            }
        }

        let notes_path = notes_dir(&self.dir, &name);
        if notes_path.exists() {
            if let Ok(entries) = std::fs::read_dir(&notes_path) {
                let mut files: Vec<_> = entries
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(OsStr::to_str) == Some("md"))
                    .collect();
                files.sort();
                for f in files {
                    let stem = f.file_stem().and_then(OsStr::to_str).unwrap_or("note");
                    let content = std::fs::read_to_string(&f).unwrap_or_default();
                    let d = diff_text(&content, "", &format!("{name}/notes/{stem}.md"));
                    if !d.is_empty() {
                        diffs.push(d);
                    }
                }
            }
        }

        std::fs::remove_dir_all(&dir)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;

        let msg = format!("deleted plan {}", name);
        if diffs.is_empty() {
            result_text(msg)
        } else {
            result_text(format!(
                "{}

{}",
                msg,
                diffs.join(
                    "

"
                )
            ))
        }
    }

    async fn handle_list_notes(
        &self,
        params: ListNotesParams,
    ) -> Result<CallToolResult, ErrorData> {
        let plan =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let mut notes = Vec::new();
        let dir = notes_dir(&self.dir, &plan);
        if dir.exists() {
            let mut entries = std::fs::read_dir(&dir)
                .map_err(|err| ErrorData::internal_error(err.to_string(), None))?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(OsStr::to_str) == Some("md"))
                .collect::<Vec<_>>();
            entries.sort();
            for path in entries {
                let content = std::fs::read_to_string(&path)
                    .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
                let (front, body) = parse_note_frontmatter(&content)
                    .map_err(|err| ErrorData::internal_error(err, None))?;
                notes.push(json!({
                    "id": display_note_id(&front.id),
                    "summary": front.summary,
                    "author": front.author,
                    "created_at": front.created_at,
                    "updated_at": front.updated_at,
                    "body": body,
                }));
            }
        }
        result_json(Value::Array(notes))
    }

    async fn handle_add_note(&self, params: AddNoteParams) -> Result<CallToolResult, ErrorData> {
        let plan =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        std::fs::create_dir_all(notes_dir(&self.dir, &plan))
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        let id = params
            .id
            .as_deref()
            .map(validate_id)
            .transpose()
            .map_err(|err| ErrorData::invalid_params(err, None))?
            .unwrap_or_else(gen_id);
        if note_file_path(&self.dir, &plan, &id).exists() {
            return Err(ErrorData::invalid_params(
                format!("note '{}' already exists in plan '{}'", id, plan),
                None,
            ));
        }
        let note = NoteRecord {
            front: NoteFrontMatter {
                id,
                summary: params.summary,
                author: params.author,
                created_at: now_iso(),
                updated_at: None,
            },
            body: params.body,
        };
        write_note(&self.dir, &plan, &note).map_err(|err| ErrorData::internal_error(err, None))?;
        let serialized =
            serialize_note(&note).map_err(|err| ErrorData::internal_error(err, None))?;
        let diff = diff_text(
            "",
            &serialized,
            &format!("{}/notes/{}.md", plan, display_note_id(&note.front.id)),
        );
        let msg = format!(
            "added note {} to plan {}",
            display_note_id(&note.front.id),
            plan
        );
        if diff.is_empty() {
            result_text(msg)
        } else {
            result_text(format!(
                "{msg}

{diff}"
            ))
        }
    }

    async fn handle_get_note(&self, params: GetNoteParams) -> Result<CallToolResult, ErrorData> {
        let plan =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let path = note_file_path(&self.dir, &plan, &params.note_id);
        if !path.exists() {
            return Err(ErrorData::invalid_params(
                format!(
                    "note {} not found in plan '{}'",
                    display_note_id(&params.note_id),
                    plan
                ),
                None,
            ));
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        let (front, body) =
            parse_note_frontmatter(&content).map_err(|err| ErrorData::internal_error(err, None))?;
        result_json(json!({
            "id": display_note_id(&front.id),
            "summary": front.summary,
            "author": front.author,
            "created_at": front.created_at,
            "updated_at": front.updated_at,
            "body": body,
        }))
    }

    async fn handle_delete_note(
        &self,
        params: DeleteNoteParams,
    ) -> Result<CallToolResult, ErrorData> {
        let plan =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let path = note_file_path(&self.dir, &plan, &params.note_id);
        if !path.exists() {
            return Err(ErrorData::invalid_params(
                format!(
                    "note {} not found in plan '{}'",
                    display_note_id(&params.note_id),
                    plan
                ),
                None,
            ));
        }
        let before_content = std::fs::read_to_string(&path)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        std::fs::remove_file(&path)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        let diff = diff_text(
            &before_content,
            "",
            &format!("{}/notes/{}.md", plan, normalize_id(&params.note_id)),
        );
        let msg = format!("deleted note {}", display_note_id(&params.note_id));
        if diff.is_empty() {
            result_text(msg)
        } else {
            result_text(format!(
                "{msg}

{diff}"
            ))
        }
    }

    async fn handle_update_note(
        &self,
        params: UpdateNoteParams,
    ) -> Result<CallToolResult, ErrorData> {
        let plan =
            validate_plan_name(&params.plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        let path = note_file_path(&self.dir, &plan, &params.note_id);
        if !path.exists() {
            return Err(ErrorData::invalid_params(
                format!(
                    "note {} not found in plan '{}'",
                    display_note_id(&params.note_id),
                    plan
                ),
                None,
            ));
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        let (mut front, mut body) =
            parse_note_frontmatter(&content).map_err(|err| ErrorData::internal_error(err, None))?;

        if let Some(summary) = params.summary {
            front.summary = Some(summary);
        }
        if let Some(author) = params.author {
            front.author = Some(author);
        }

        let body_count = [
            params.replace_body.is_some(),
            params.append_body.is_some(),
            params.replace_in_body.is_some(),
        ]
        .iter()
        .filter(|&&b| b)
        .count();
        if body_count > 1 {
            return Err(ErrorData::invalid_params(
                "at most one of replace_body, append_body, replace_in_body may be provided",
                None,
            ));
        }
        let before_body = body.clone();
        if let Some(rb) = params.replace_body {
            body = rb;
        } else if let Some(ab) = params.append_body {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(&ab);
        } else if let Some(ri) = params.replace_in_body {
            body = apply_replace_in(&body, &ri)?;
        }

        front.updated_at = Some(now_iso());
        let note = NoteRecord {
            front,
            body: body.clone(),
        };
        write_note(&self.dir, &plan, &note).map_err(|err| ErrorData::internal_error(err, None))?;

        let note_path = format!("{}/notes/{}.md", plan, normalize_id(&params.note_id));
        let diff = diff_text(&before_body, &body, &note_path);
        let msg = format!("updated note {}", display_note_id(&params.note_id));
        if diff.is_empty() {
            result_text(msg)
        } else {
            result_text(format!("{msg}\n\n{diff}"))
        }
    }
}

fn default_open_status() -> String {
    "open".to_string()
}

fn normalize_id(id: &str) -> String {
    let trimmed = id.trim();
    let trimmed = trimmed
        .strip_prefix("task-")
        .or_else(|| trimmed.strip_prefix("TASK-"))
        .or_else(|| trimmed.strip_prefix("note-"))
        .or_else(|| trimmed.strip_prefix("NOTE-"))
        .unwrap_or(trimmed);
    trimmed.to_ascii_lowercase()
}

fn normalize_plan_name(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace(' ', "-")
}

fn validate_plan_name(name: &str) -> Result<String, String> {
    let normalized = normalize_plan_name(name);
    if normalized.is_empty() {
        return Err("plan name must not be empty".to_string());
    }
    if normalized.contains('/') || normalized.contains('\\') || normalized.contains("..") {
        return Err(format!(
            "plan name '{}' must not contain path separators or '..'",
            name
        ));
    }
    Ok(normalized)
}

fn gen_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:08x}", (nanos & 0xffff_ffff) as u32)
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn validate_id(id: &str) -> Result<String, String> {
    let normalized = normalize_id(id);
    if normalized.is_empty()
        || normalized.len() > 64
        || !normalized
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Err(format!(
            "ID '{}' must contain only alphanumeric, hyphen, or underscore characters (1-64 chars)",
            id
        ))
    } else {
        Ok(normalized)
    }
}

fn display_id(id: &str) -> String {
    normalize_id(id)
}

fn display_note_id(id: &str) -> String {
    normalize_id(id)
}

fn result_json(value: Value) -> Result<CallToolResult, ErrorData> {
    let text = serde_json::to_string_pretty(&value)
        .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
    result_text(text)
}

fn result_text(text: String) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

fn diff_text(before: &str, after: &str, path: &str) -> String {
    if before == after {
        return String::new();
    }
    let diff = TextDiff::from_lines(before, after);
    let mut output = format!("--- a/{path}\n+++ b/{path}\n");
    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            let sign = match change.tag() {
                ChangeTag::Delete => "-",
                ChangeTag::Insert => "+",
                ChangeTag::Equal => " ",
            };
            let value = change.value();
            output.push_str(sign);
            output.push_str(value);
            if !value.ends_with('\n') {
                output.push('\n');
            }
        }
    }
    format!("```diff\n{output}```")
}

fn apply_replace_in(body: &str, r: &ReplaceInContent) -> Result<String, ErrorData> {
    if r.old_text.is_empty() {
        return Err(ErrorData::invalid_params(
            "old_text must not be empty",
            None,
        ));
    }
    if !body.contains(&*r.old_text) {
        return Err(ErrorData::invalid_params(
            format!("old_text {:?} not found in body", r.old_text),
            None,
        ));
    }
    let result = if r.replace_all == Some(true) {
        body.replace(&*r.old_text, &r.new_text)
    } else {
        body.replacen(&*r.old_text, &r.new_text, 1)
    };
    Ok(result)
}

fn parse_arguments<T: serde::de::DeserializeOwned>(
    args: Option<Map<String, Value>>,
) -> Result<T, ErrorData> {
    serde_json::from_value(Value::Object(args.unwrap_or_default()))
        .map_err(|err| ErrorData::invalid_params(err.to_string(), None))
}

fn object_schema_with_desc(properties: Vec<(&str, &str, Schema)>, required: &[&str]) -> Schema {
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("object".to_string()));

    let mut property_map = Map::new();
    for (name, desc, property_schema) in properties {
        let mut prop = property_schema.as_value().clone();
        if let Some(obj) = prop.as_object_mut() {
            obj.insert("description".to_string(), Value::String(desc.to_string()));
        }
        property_map.insert(name.to_string(), prop);
    }
    schema.insert("properties".to_string(), Value::Object(property_map));
    schema.insert("additionalProperties".to_string(), Value::Bool(false));

    if !required.is_empty() {
        schema.insert(
            "required".to_string(),
            Value::Array(
                required
                    .iter()
                    .map(|name| Value::String((*name).to_string()))
                    .collect(),
            ),
        );
    }

    schema.into()
}
macro_rules! impl_json_schema {
    ($type:ty, $title:expr, $properties_fn:expr, $required:expr) => {
        impl JsonSchema for $type {
            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed($title)
            }

            fn schema_id() -> Cow<'static, str> {
                Cow::Borrowed(concat!(module_path!(), "::", $title))
            }

            fn json_schema(gen: &mut SchemaGenerator) -> Schema {
                object_schema_with_desc($properties_fn(gen), $required)
            }
        }
    };
}

impl_json_schema!(
    ReplaceInContent,
    "ReplaceInContent",
    |gen: &mut SchemaGenerator| vec![
        (
            "old_text",
            "Text to find and replace",
            gen.subschema_for::<String>()
        ),
        (
            "new_text",
            "Replacement text",
            gen.subschema_for::<String>()
        ),
        (
            "replace_all",
            "If true, replace all occurrences; default replaces first only",
            gen.subschema_for::<Option<bool>>()
        ),
    ],
    &["old_text", "new_text"]
);

impl_json_schema!(
    ListTasksParams,
    "ListTasksParams",
    |gen: &mut SchemaGenerator| vec![
        ("plan", "Plan name", gen.subschema_for::<String>()),
        (
            "filter",
            "Filter by status: 'open' (default), 'closed', or 'all'",
            gen.subschema_for::<String>()
        ),
        (
            "tag",
            "Optional tag filter",
            gen.subschema_for::<Option<String>>()
        ),
    ],
    &["plan"]
);

impl_json_schema!(
    GetTaskParams,
    "GetTaskParams",
    |gen: &mut SchemaGenerator| vec![
        ("plan", "Plan name", gen.subschema_for::<String>()),
        ("id", "Task ID", gen.subschema_for::<String>()),
    ],
    &["plan", "id"]
);

impl_json_schema!(
    AddTaskParams,
    "AddTaskParams",
    |gen: &mut SchemaGenerator| vec![
        ("title", "Short title", gen.subschema_for::<String>()),
        ("plan", "Plan name", gen.subschema_for::<String>()),
        (
            "id",
            "Optional task ID",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "summary",
            "Optional summary",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "author",
            "Optional author",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "assignee",
            "Optional assignee",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "executor",
            "Optional executor",
            gen.subschema_for::<Option<String>>()
        ),
        ("tags", "Optional tags", gen.subschema_for::<Vec<String>>()),
        (
            "status",
            "Initial status (default: 'open')",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "body",
            "Optional markdown body",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "dependencies",
            "List of dependency keys or IDs",
            gen.subschema_for::<Vec<String>>()
        ),
    ],
    &["title", "plan"]
);

impl_json_schema!(
    UpdateTaskParams,
    "UpdateTaskParams",
    |gen: &mut SchemaGenerator| {
        vec![
        ("plan", "Plan name", gen.subschema_for::<String>()),
        ("id", "Task ID", gen.subschema_for::<String>()),
        (
            "title",
            "Optional title",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "summary",
            "Optional summary",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "author",
            "Optional author",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "assignee",
            "Optional assignee",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "executor",
            "Optional executor",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "tags",
            "Replace tags with provided list",
            gen.subschema_for::<Option<Vec<String>>>()
        ),
        (
            "status",
            "Optional new status",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "replace_body",
            "Replace entire task body with this content. Keep under 1000 words.",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "append_body",
            "Append text to task body. Auto-inserts newline separator if needed. Keep under 1000 words.",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "replace_in_body",
            "Surgically replace text in task body.",
            gen.subschema_for::<Option<ReplaceInContent>>()
        ),
        (
            "dependencies",
            "Replace dependencies with provided list",
            gen.subschema_for::<Option<Vec<String>>>()
        ),
    ]
    },
    &["plan", "id"]
);

impl_json_schema!(
    DeleteTaskParams,
    "DeleteTaskParams",
    |gen: &mut SchemaGenerator| vec![
        ("plan", "Plan name", gen.subschema_for::<String>()),
        ("id", "Task ID", gen.subschema_for::<String>()),
    ],
    &["plan", "id"]
);

impl_json_schema!(
    ListPlansParams,
    "ListPlansParams",
    |_gen: &mut SchemaGenerator| vec![],
    &[]
);

impl_json_schema!(
    AddPlanParams,
    "AddPlanParams",
    |gen: &mut SchemaGenerator| vec![
        ("name", "Plan name", gen.subschema_for::<String>()),
        (
            "title",
            "Optional title",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "summary",
            "Optional summary",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "author",
            "Optional author",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "assignee",
            "Optional assignee",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "executor",
            "Optional executor",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "git_branch",
            "Optional git branch",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "github_owner_repo",
            "Optional owner/repo",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "body",
            "Optional markdown body",
            gen.subschema_for::<Option<String>>()
        ),
    ],
    &["name"]
);

impl_json_schema!(
    GetPlanParams,
    "GetPlanParams",
    |gen: &mut SchemaGenerator| vec![("name", "Plan name", gen.subschema_for::<String>()),],
    &["name"]
);

impl_json_schema!(
    UpdatePlanParams,
    "UpdatePlanParams",
    |gen: &mut SchemaGenerator| {
        vec![
        ("name", "Plan name", gen.subschema_for::<String>()),
        (
            "replace_content",
            "Replace entire plan body with this content. Keep under 1000 words.",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "append_content",
            "Append text to plan body. Auto-inserts newline separator if needed. Keep under 1000 words.",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "replace_in_content",
            "Surgically replace text in plan body.",
            gen.subschema_for::<Option<ReplaceInContent>>()
        ),
        (
            "title",
            "Optional title",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "summary",
            "Optional summary",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "author",
            "Optional author",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "assignee",
            "Optional assignee",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "executor",
            "Optional executor",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "git_branch",
            "Optional git branch",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "github_owner_repo",
            "Optional owner/repo",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "tasks",
            "Optional tasks to create in batch",
            gen.subschema_for::<Option<Vec<TaskSpec>>>()
        ),
    ]
    },
    &["name"]
);

impl_json_schema!(
    DeletePlanParams,
    "DeletePlanParams",
    |gen: &mut SchemaGenerator| vec![("name", "Plan name", gen.subschema_for::<String>()),],
    &["name"]
);

impl_json_schema!(
    ListNotesParams,
    "ListNotesParams",
    |gen: &mut SchemaGenerator| vec![("plan", "Plan name", gen.subschema_for::<String>()),],
    &["plan"]
);

impl_json_schema!(
    AddNoteParams,
    "AddNoteParams",
    |gen: &mut SchemaGenerator| vec![
        ("plan", "Plan name", gen.subschema_for::<String>()),
        (
            "id",
            "Optional note ID",
            gen.subschema_for::<Option<String>>()
        ),
        ("body", "Note markdown body", gen.subschema_for::<String>()),
        (
            "summary",
            "Optional note summary",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "author",
            "Optional note author",
            gen.subschema_for::<Option<String>>()
        ),
    ],
    &["plan", "body"]
);

impl_json_schema!(
    GetNoteParams,
    "GetNoteParams",
    |gen: &mut SchemaGenerator| vec![
        ("plan", "Plan name", gen.subschema_for::<String>()),
        ("note_id", "Note ID", gen.subschema_for::<String>()),
    ],
    &["plan", "note_id"]
);

impl_json_schema!(
    DeleteNoteParams,
    "DeleteNoteParams",
    |gen: &mut SchemaGenerator| vec![
        ("plan", "Plan name", gen.subschema_for::<String>()),
        ("note_id", "Note ID", gen.subschema_for::<String>()),
    ],
    &["plan", "note_id"]
);

impl_json_schema!(
    UpdateNoteParams,
    "UpdateNoteParams",
    |gen: &mut SchemaGenerator| {
        vec![
        ("plan", "Plan name", gen.subschema_for::<String>()),
        ("note_id", "Note ID", gen.subschema_for::<String>()),
        ("summary", "Optional note summary", gen.subschema_for::<Option<String>>()),
        ("author", "Optional note author", gen.subschema_for::<Option<String>>()),
        ("replace_body", "Replace entire note body with this content. Keep under 1000 words.", gen.subschema_for::<Option<String>>()),
        ("append_body", "Append text to note body. Auto-inserts newline separator if needed. Keep under 1000 words.", gen.subschema_for::<Option<String>>()),
        ("replace_in_body", "Surgically replace text in note body.", gen.subschema_for::<Option<ReplaceInContent>>()),
    ]
    },
    &["plan", "note_id"]
);

impl_json_schema!(
    TaskSpec,
    "TaskSpec",
    |gen: &mut SchemaGenerator| vec![
        ("title", "Task title", gen.subschema_for::<String>()),
        (
            "id",
            "Optional task ID",
            gen.subschema_for::<Option<String>>()
        ),
        ("tags", "Task tags", gen.subschema_for::<Vec<String>>()),
        (
            "dependencies",
            "Task dependencies",
            gen.subschema_for::<Vec<String>>()
        ),
        (
            "status",
            "Task status",
            gen.subschema_for::<Option<String>>()
        ),
        ("body", "Task body", gen.subschema_for::<Option<String>>()),
        (
            "summary",
            "Task summary",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "author",
            "Task author",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "assignee",
            "Task assignee",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "executor",
            "Task executor",
            gen.subschema_for::<Option<String>>()
        ),
    ],
    &["title"]
);

fn plan_dir(dir: &Path, plan_name: &str) -> PathBuf {
    dir.join(plan_name)
}

fn plan_file_path(dir: &Path, plan_name: &str) -> PathBuf {
    plan_dir(dir, plan_name).join("plan.md")
}

fn tasks_dir(dir: &Path, plan_name: &str) -> PathBuf {
    plan_dir(dir, plan_name).join("tasks")
}

fn task_file_path(dir: &Path, plan_name: &str, id: &str) -> PathBuf {
    tasks_dir(dir, plan_name).join(format!("{}.md", normalize_id(id)))
}

fn notes_dir(dir: &Path, plan_name: &str) -> PathBuf {
    plan_dir(dir, plan_name).join("notes")
}

fn note_file_path(dir: &Path, plan_name: &str, id: &str) -> PathBuf {
    notes_dir(dir, plan_name).join(format!("{}.md", normalize_id(id)))
}

fn serialize_task(task: &TaskRecord) -> Result<String, String> {
    let yaml = serde_yaml::to_string(&task.front).map_err(|err| err.to_string())?;
    Ok(format!("---\n{}---\n{}", yaml, task.body))
}

fn parse_task_frontmatter(content: &str) -> Result<(TaskFrontMatter, String), String> {
    let rest = content
        .strip_prefix("---\n")
        .ok_or_else(|| "missing YAML front matter".to_string())?;
    let (front, body) = rest
        .split_once("\n---\n")
        .ok_or_else(|| "missing YAML front matter terminator".to_string())?;
    let front = serde_yaml::from_str(front).map_err(|err| err.to_string())?;
    Ok((front, body.to_string()))
}

fn serialize_plan(record: &PlanRecord) -> Result<String, String> {
    let yaml = serde_yaml::to_string(&record.front).map_err(|err| err.to_string())?;
    Ok(format!("---\n{}---\n{}", yaml, record.body))
}

fn parse_plan_frontmatter(
    content: &str,
    plan_name: &str,
) -> Result<(PlanFrontMatter, String), String> {
    let Some(rest) = content.strip_prefix("---\n") else {
        return Ok((
            PlanFrontMatter {
                id: plan_name.to_string(),
                created_at: "".to_string(),
                ..Default::default()
            },
            content.to_string(),
        ));
    };
    let Some((front, body)) = rest.split_once("\n---\n") else {
        return Err("missing YAML front matter terminator".to_string());
    };
    let front = serde_yaml::from_str(front).map_err(|err| err.to_string())?;
    Ok((front, body.to_string()))
}

fn serialize_note(record: &NoteRecord) -> Result<String, String> {
    let yaml = serde_yaml::to_string(&record.front).map_err(|err| err.to_string())?;
    Ok(format!("---\n{}---\n{}", yaml, record.body))
}

fn parse_note_frontmatter(content: &str) -> Result<(NoteFrontMatter, String), String> {
    let rest = content
        .strip_prefix("---\n")
        .ok_or_else(|| "missing YAML front matter".to_string())?;
    let (front, body) = rest
        .split_once("\n---\n")
        .ok_or_else(|| "missing YAML front matter terminator".to_string())?;
    let front = serde_yaml::from_str(front).map_err(|err| err.to_string())?;
    Ok((front, body.to_string()))
}

fn write_task(dir: &Path, task: &TaskRecord) -> Result<(), String> {
    let tasks = tasks_dir(dir, &task.front.plan);
    std::fs::create_dir_all(&tasks).map_err(|err| err.to_string())?;
    let content = serialize_task(task)?;
    let final_path = task_file_path(dir, &task.front.plan, &task.front.id);
    let tmp_path = final_path.with_extension("tmp");
    std::fs::write(&tmp_path, &content).map_err(|err| err.to_string())?;
    std::fs::rename(&tmp_path, &final_path).map_err(|err| err.to_string())?;
    Ok(())
}

fn write_plan_file(path: &Path, content: &str) -> Result<(), String> {
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, content).map_err(|err| err.to_string())?;
    std::fs::rename(&tmp_path, path).map_err(|err| err.to_string())?;
    Ok(())
}

fn write_note(dir: &Path, plan_name: &str, note: &NoteRecord) -> Result<(), String> {
    let notes = notes_dir(dir, plan_name);
    std::fs::create_dir_all(&notes).map_err(|err| err.to_string())?;
    let content = serialize_note(note)?;
    let final_path = note_file_path(dir, plan_name, &note.front.id);
    let tmp_path = final_path.with_extension("tmp");
    std::fs::write(&tmp_path, &content).map_err(|err| err.to_string())?;
    std::fs::rename(&tmp_path, &final_path).map_err(|err| err.to_string())?;
    Ok(())
}

fn read_task(dir: &Path, plan_name: &str, id: &str) -> Result<TaskRecord, String> {
    let path = task_file_path(dir, plan_name, id);
    if !path.exists() {
        return Err(format!(
            "task {} not found in plan '{}'",
            display_id(id),
            plan_name
        ));
    }
    let content = std::fs::read_to_string(&path).map_err(|err| err.to_string())?;
    let (front, body) = parse_task_frontmatter(&content)?;
    Ok(TaskRecord { front, body })
}

fn list_tasks(
    dir: &Path,
    plan_filter: Option<&str>,
    tag_filter: Option<&str>,
    status_filter: Option<&str>,
) -> Vec<TaskRecord> {
    let mut tasks = Vec::new();
    let plans: Vec<String> = if let Some(plan) = plan_filter {
        vec![normalize_plan_name(plan)]
    } else {
        plan_dirs(dir)
            .into_iter()
            .filter_map(|path| path.file_name().and_then(OsStr::to_str).map(str::to_string))
            .collect()
    };

    for plan in plans {
        let tasks_path = tasks_dir(dir, &plan);
        let Ok(entries) = std::fs::read_dir(&tasks_path) else {
            continue;
        };
        let mut files = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(OsStr::to_str) == Some("md"))
            .collect::<Vec<_>>();
        files.sort();
        for path in files {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok((front, body)) = parse_task_frontmatter(&content) else {
                continue;
            };
            if let Some(status) = status_filter {
                if front.status != status {
                    continue;
                }
            }
            if let Some(tag) = tag_filter {
                if !front.tags.iter().any(|candidate| candidate == tag) {
                    continue;
                }
            }
            tasks.push(TaskRecord { front, body });
        }
    }
    tasks
}

fn plan_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    dirs.sort();
    dirs
}

fn plan_last_activity(plan_dir: &Path) -> std::io::Result<std::time::SystemTime> {
    let mut latest = None;

    let plan_file = plan_dir.join("plan.md");
    if let Ok(metadata) = std::fs::metadata(&plan_file) {
        latest = Some(metadata.modified()?);
    }

    for subdir in ["tasks", "notes"] {
        let dir = plan_dir.join(subdir);
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };

        for entry in entries {
            let path = entry?.path();
            if !path.is_file() || path.extension().and_then(OsStr::to_str) != Some("md") {
                continue;
            }

            let modified = std::fs::metadata(&path)?.modified()?;
            latest = Some(match latest {
                Some(current) => current.max(modified),
                None => modified,
            });
        }
    }

    match latest {
        Some(modified) => Ok(modified),
        None => plan_dir.metadata()?.modified(),
    }
}

async fn run_cleanup_pass(dir: &Path, retention: Duration) {
    let dir_owned = dir.to_owned();
    let dirs = match tokio::task::spawn_blocking(move || plan_dirs(&dir_owned)).await {
        Ok(dirs) => dirs,
        Err(e) => {
            eprintln!("[cleanup] error listing plans: {e}");
            return;
        }
    };

    for plan_dir in dirs {
        let name = plan_dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let plan_dir_for_activity = plan_dir.clone();
        let last_activity =
            match tokio::task::spawn_blocking(move || plan_last_activity(&plan_dir_for_activity))
                .await
            {
                Ok(Ok(last_activity)) => last_activity,
                Ok(Err(e)) => {
                    eprintln!("[cleanup] error checking plan {name}: {e}");
                    continue;
                }
                Err(e) => {
                    eprintln!("[cleanup] error checking plan {name}: {e}");
                    continue;
                }
            };

        let age = std::time::SystemTime::now()
            .duration_since(last_activity)
            .unwrap_or_default();
        if age <= retention {
            continue;
        }

        let plan_dir_for_delete = plan_dir.clone();
        match tokio::task::spawn_blocking(move || std::fs::remove_dir_all(plan_dir_for_delete))
            .await
        {
            Ok(Ok(())) => {
                eprintln!(
                    "[cleanup] deleted inactive plan {name} (inactive for {} days)",
                    age.as_secs() / 86_400
                );
            }
            Ok(Err(e)) => eprintln!("[cleanup] error deleting plan {name}: {e}"),
            Err(e) => eprintln!("[cleanup] error deleting plan {name}: {e}"),
        }
    }
}

pub async fn cleanup_loop(dir: PathBuf, retention_days: u64) {
    let retention = Duration::from_secs(retention_days.saturating_mul(86_400));

    run_cleanup_pass(&dir, retention).await;

    let mut interval = tokio::time::interval(Duration::from_secs(86_400));
    interval.tick().await;

    loop {
        interval.tick().await;
        run_cleanup_pass(&dir, retention).await;
    }
}

fn list_note_ids(dir: &Path, plan_name: &str) -> Vec<String> {
    let notes = notes_dir(dir, plan_name);
    let Ok(entries) = std::fs::read_dir(notes) else {
        return Vec::new();
    };
    let mut ids = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(OsStr::to_str) == Some("md"))
        .filter_map(|path| {
            path.file_stem()
                .and_then(OsStr::to_str)
                .map(display_note_id)
        })
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

impl ServerHandler for PlansServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "harnx-mcp-plans",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "File-based plan/task/note management server using markdown + YAML front matter",
            )
    }

    async fn list_tools(
        &self,
        _pagination: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            meta: None,
            tools: vec![
                Tool::new("list_plans", "List all plans with metadata and task/note counts.", Map::new())
                    .with_input_schema::<ListPlansParams>()
                    .with_meta(Meta(json!({"call_template": "list plans", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("add_plan", "Create a new plan with optional metadata. Keep body content under 1000 words per call; use update_plan with replace_in_content for targeted edits.", Map::new())
                    .with_input_schema::<AddPlanParams>()
                    .with_meta(Meta(json!({"call_template": "create plan {{ args.name }}{% if args.title %} — {{ args.title | truncate(40) }}{% endif %}{% if args.git_branch %} [{{ args.git_branch }}]{% endif %}{% if args.github_owner_repo %} ({{ args.github_owner_repo }}){% endif %}{% if args.body %}
{{ args.body | truncate(80) }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("get_plan", "Read plan metadata, body, and list task/note IDs.", Map::new())
                    .with_input_schema::<GetPlanParams>()
                    .with_meta(Meta(json!({"call_template": "read plan {{ args.name }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("update_plan", "Update plan body and metadata. Creates plan if it doesn't exist. Use replace_content to rewrite body, append_content to extend it, or replace_in_content for surgical edits. Optionally batch-create tasks. Keep each write under 1000 words.", Map::new())
                    .with_input_schema::<UpdatePlanParams>()
                    .with_meta(Meta(json!({"call_template": "update plan {{ args.name }}{% if args.title %} — {{ args.title | truncate(40) }}{% endif %}{% if args.git_branch %} [{{ args.git_branch }}]{% endif %}{% if args.github_owner_repo %} ({{ args.github_owner_repo }}){% endif %}{% if args.tasks %} [{{ args.tasks | length }} tasks]{% endif %}{% if args.replace_content %}
{{ args.replace_content | truncate(80) }}{% endif %}{% if args.append_content %}
+{{ args.append_content | truncate(80) }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("delete_plan", "Delete an entire plan and all its tasks and notes.", Map::new())
                    .with_input_schema::<DeletePlanParams>()
                    .with_meta(Meta(json!({"call_template": "delete plan {{ args.name }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("list_tasks", "List tasks in a plan with optional filters.", Map::new())
                    .with_input_schema::<ListTasksParams>()
                    .with_meta(Meta(json!({"call_template": "list tasks {{ args.plan }}{% if args.filter and args.filter != 'open' %} [{{ args.filter }}]{% endif %}{% if args.tag %} #{{ args.tag }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("add_task", "Create a task in a plan. Keep body under 1000 words; use update_task with replace_in_body for targeted edits.", Map::new())
                    .with_input_schema::<AddTaskParams>()
                    .with_meta(Meta(json!({"call_template": "create task {{ args.plan }}/{{ args.title }}{% if args.status %} [{{ args.status }}]{% endif %}{% if args.assignee %} @{{ args.assignee }}{% endif %}{% if args.executor %} ▶{{ args.executor }}{% endif %}{% if args.tags %} #{{ args.tags | join(' #') }}{% endif %}{% if args.body %}
{{ args.body | truncate(80) }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("get_task", "Read a task by ID within a plan.", Map::new())
                    .with_input_schema::<GetTaskParams>()
                    .with_meta(Meta(json!({"call_template": "read task {{ args.plan }}/{{ args.id }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("update_task", "Update a task within its plan. Use replace_body to rewrite body, append_body to extend it, or replace_in_body for surgical edits. Keep each write under 1000 words.", Map::new())
                    .with_input_schema::<UpdateTaskParams>()
                    .with_meta(Meta(json!({"call_template": "update task {{ args.plan }}/{{ args.id }}{% if args.title %} — {{ args.title | truncate(40) }}{% endif %}{% if args.status %} [{{ args.status }}]{% endif %}{% if args.assignee %} @{{ args.assignee }}{% endif %}{% if args.executor %} ▶{{ args.executor }}{% endif %}{% if args.tags %} #{{ args.tags | join(' #') }}{% endif %}{% if args.replace_body %}
{{ args.replace_body | truncate(80) }}{% endif %}{% if args.append_body %}
+{{ args.append_body | truncate(80) }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("delete_task", "Delete a task by ID.", Map::new())
                    .with_input_schema::<DeleteTaskParams>()
                    .with_meta(Meta(json!({"call_template": "delete task {{ args.plan }}/{{ args.id }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("list_notes", "List notes for a plan.", Map::new())
                    .with_input_schema::<ListNotesParams>()
                    .with_meta(Meta(json!({"call_template": "list notes {{ args.plan }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("add_note", "Add a note to a plan. Keep body under 1000 words; use update_note with replace_in_body for targeted edits.", Map::new())
                    .with_input_schema::<AddNoteParams>()
                    .with_meta(Meta(json!({"call_template": "add note {{ args.plan }}{% if args.summary %} — {{ args.summary | truncate(60) }}{% endif %}{% if args.author %} by {{ args.author }}{% endif %}{% if args.body %}
{{ args.body | truncate(80) }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("get_note", "Read a note from a plan.", Map::new())
                    .with_input_schema::<GetNoteParams>()
                    .with_meta(Meta(json!({"call_template": "read note {{ args.plan }}/{{ args.note_id }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("update_note", "Update a note within a plan. Use replace_body, append_body, or replace_in_body for body edits. Keep each write under 1000 words.", Map::new())
                    .with_input_schema::<UpdateNoteParams>()
                    .with_meta(Meta(json!({"call_template": "update note {{ args.plan }}/{{ args.note_id }}{% if args.summary %} — {{ args.summary | truncate(60) }}{% endif %}{% if args.replace_body %}
{{ args.replace_body | truncate(80) }}{% endif %}{% if args.append_body %}
+{{ args.append_body | truncate(80) }}{% endif %}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("delete_note", "Delete a note from a plan.", Map::new())
                    .with_input_schema::<DeleteNoteParams>()
                    .with_meta(Meta(json!({"call_template": "delete note {{ args.plan }}/{{ args.note_id }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
            ],
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        match request.name.as_ref() {
            "list_plans" => {
                let _params = parse_arguments::<ListPlansParams>(request.arguments)?;
                self.handle_list_plans().await
            }
            "add_plan" => {
                let params = parse_arguments::<AddPlanParams>(request.arguments)?;
                self.handle_add_plan(params).await
            }
            "get_plan" => {
                let params = parse_arguments::<GetPlanParams>(request.arguments)?;
                self.handle_get_plan(params).await
            }
            "update_plan" => {
                let params = parse_arguments::<UpdatePlanParams>(request.arguments)?;
                self.handle_update_plan(params).await
            }
            "delete_plan" => {
                let params = parse_arguments::<DeletePlanParams>(request.arguments)?;
                self.handle_delete_plan(params).await
            }
            "list_tasks" => {
                let params = parse_arguments::<ListTasksParams>(request.arguments)?;
                self.handle_list_tasks(params).await
            }
            "add_task" => {
                let params = parse_arguments::<AddTaskParams>(request.arguments)?;
                self.handle_add_task(params).await
            }
            "get_task" => {
                let params = parse_arguments::<GetTaskParams>(request.arguments)?;
                self.handle_get_task(params).await
            }
            "update_task" => {
                let params = parse_arguments::<UpdateTaskParams>(request.arguments)?;
                self.handle_update_task(params).await
            }
            "delete_task" => {
                let params = parse_arguments::<DeleteTaskParams>(request.arguments)?;
                self.handle_delete_task(params).await
            }
            "list_notes" => {
                let params = parse_arguments::<ListNotesParams>(request.arguments)?;
                self.handle_list_notes(params).await
            }
            "add_note" => {
                let params = parse_arguments::<AddNoteParams>(request.arguments)?;
                self.handle_add_note(params).await
            }
            "get_note" => {
                let params = parse_arguments::<GetNoteParams>(request.arguments)?;
                self.handle_get_note(params).await
            }
            "update_note" => {
                let params = parse_arguments::<UpdateNoteParams>(request.arguments)?;
                self.handle_update_note(params).await
            }
            "delete_note" => {
                let params = parse_arguments::<DeleteNoteParams>(request.arguments)?;
                self.handle_delete_note(params).await
            }
            other => Err(ErrorData::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs;
    use std::time::Duration;

    fn temp_test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("harnx-mcp-plans-{}-{}", label, gen_id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn extract_text(result: CallToolResult) -> String {
        result.content[0]
            .raw
            .as_text()
            .map(|text| text.text.clone())
            .unwrap_or_else(|| panic!("unexpected content: {:?}", result.content[0]))
    }

    fn extract_id(summary: &str) -> String {
        summary.split_whitespace().nth(2).unwrap().to_string()
    }

    #[test]
    fn plan_last_activity_uses_latest_file_mtime_not_dir_mtime() {
        let dir = temp_test_dir("plan-last-activity-latest-file");
        let plan_dir = dir.join("plan-a");
        fs::create_dir_all(&plan_dir).unwrap();

        let dir_mtime = plan_dir.metadata().unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(50));

        fs::write(plan_dir.join("plan.md"), "plan").unwrap();
        let plan_mtime = plan_dir
            .join("plan.md")
            .metadata()
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));

        fs::create_dir_all(plan_dir.join("tasks")).unwrap();
        fs::write(plan_dir.join("tasks/task-1.md"), "task").unwrap();
        let task_mtime = plan_dir
            .join("tasks/task-1.md")
            .metadata()
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));

        fs::create_dir_all(plan_dir.join("notes")).unwrap();
        fs::write(plan_dir.join("notes/note-1.md"), "note").unwrap();
        let note_mtime = plan_dir
            .join("notes/note-1.md")
            .metadata()
            .unwrap()
            .modified()
            .unwrap();

        let actual = plan_last_activity(&plan_dir).unwrap();
        let expected = plan_mtime.max(task_mtime).max(note_mtime);

        assert_eq!(actual, expected);
        assert!(actual > dir_mtime);
    }

    #[test]
    fn plan_last_activity_falls_back_to_dir_mtime_for_empty_plan() {
        let dir = temp_test_dir("plan-last-activity-empty-plan");
        let plan_dir = dir.join("plan-a");
        fs::create_dir_all(&plan_dir).unwrap();

        let expected = plan_dir.metadata().unwrap().modified().unwrap();
        let actual = plan_last_activity(&plan_dir).unwrap();

        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn add_and_get_task() {
        let dir = temp_test_dir("add-and-get-task");
        let server = PlansServer::new(dir);

        let add = server
            .handle_add_task(AddTaskParams {
                title: "Task 1".to_string(),
                plan: "plan-a".to_string(),
                summary: Some("sum".to_string()),
                author: Some("author".to_string()),
                assignee: None,
                executor: None,
                tags: vec!["rust".to_string()],
                status: None,
                body: Some("body".to_string()),
                id: None,
                dependencies: vec![],
            })
            .await
            .unwrap();
        let id = extract_id(&extract_text(add));

        let got = server
            .handle_get_task(GetTaskParams {
                plan: "plan-a".to_string(),
                id,
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
        assert_eq!(value["title"], "Task 1");
        assert_eq!(value["summary"], "sum");
        assert_eq!(value["body"], "body");
    }

    #[tokio::test]
    async fn add_task_with_agent_id() {
        let dir = temp_test_dir("add-task-agent-id");
        let server = PlansServer::new(dir.clone());

        let add = server
            .handle_add_task(AddTaskParams {
                title: "Agent ID Task".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: None,
                id: Some("my-task-id".to_string()),
                dependencies: vec![],
            })
            .await
            .unwrap();
        let returned_id = extract_id(&extract_text(add));
        assert_eq!(returned_id, "my-task-id");

        let path = dir.join("plan-a").join("tasks").join("my-task-id.md");
        assert!(path.exists(), "task file should exist at my-task-id.md");
    }

    #[tokio::test]
    async fn add_task_duplicate_id_error() {
        let dir = temp_test_dir("add-task-dup-id");
        let server = PlansServer::new(dir);

        server
            .handle_add_task(AddTaskParams {
                title: "First".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: None,
                id: Some("dup-id".to_string()),
                dependencies: vec![],
            })
            .await
            .unwrap();

        let err = server
            .handle_add_task(AddTaskParams {
                title: "Second".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: None,
                id: Some("dup-id".to_string()),
                dependencies: vec![],
            })
            .await
            .unwrap_err();
        assert!(
            err.message.contains("already exists"),
            "expected 'already exists' in: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn add_task_invalid_id_rejected() {
        let dir = temp_test_dir("add-task-invalid-id");
        let server = PlansServer::new(dir);

        // Slash in ID
        let err = server
            .handle_add_task(AddTaskParams {
                title: "Bad ID".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: None,
                id: Some("bad/id".to_string()),
                dependencies: vec![],
            })
            .await
            .unwrap_err();
        assert!(
            err.message.contains("alphanumeric") || err.message.contains("1-64"),
            "expected validation error, got: {}",
            err.message
        );

        // Empty ID
        let err2 = server
            .handle_add_task(AddTaskParams {
                title: "Empty ID".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: None,
                id: Some("".to_string()),
                dependencies: vec![],
            })
            .await
            .unwrap_err();
        assert!(
            err2.message.contains("alphanumeric") || err2.message.contains("1-64"),
            "expected validation error, got: {}",
            err2.message
        );
    }

    #[tokio::test]
    async fn add_task_auto_id_fallback() {
        let dir = temp_test_dir("add-task-auto-id");
        let server = PlansServer::new(dir);

        let add = server
            .handle_add_task(AddTaskParams {
                title: "Auto ID".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: None,
                id: None,
                dependencies: vec![],
            })
            .await
            .unwrap();
        let id = extract_id(&extract_text(add));
        assert!(!id.is_empty(), "auto-generated ID should not be empty");
    }

    #[tokio::test]
    async fn add_note_with_agent_id() {
        let dir = temp_test_dir("add-note-agent-id");
        let server = PlansServer::new(dir.clone());

        let add = server
            .handle_add_note(AddNoteParams {
                plan: "plan-a".to_string(),
                id: Some("my-note-id".to_string()),
                body: "note body".to_string(),
                summary: None,
                author: None,
            })
            .await
            .unwrap();
        let text = extract_text(add);
        assert!(
            text.contains("my-note-id"),
            "result should mention my-note-id, got: {}",
            text
        );

        let path = dir.join("plan-a").join("notes").join("my-note-id.md");
        assert!(path.exists(), "note file should exist at my-note-id.md");
    }

    #[tokio::test]
    async fn add_note_invalid_id_rejected() {
        let dir = temp_test_dir("add-note-invalid-id");
        let server = PlansServer::new(dir);

        let err = server
            .handle_add_note(AddNoteParams {
                plan: "plan-a".to_string(),
                id: Some("bad/id".to_string()),
                body: "note body".to_string(),
                summary: None,
                author: None,
            })
            .await
            .unwrap_err();
        assert!(
            err.message.contains("alphanumeric") || err.message.contains("1-64"),
            "expected validation error, got: {}",
            err.message
        );
    }

    #[test]
    fn validate_id_rejects_invalid() {
        assert!(validate_id("").is_err(), "empty string should be rejected");
        assert!(validate_id("bad/id").is_err(), "slash should be rejected");
        assert!(validate_id("bad id").is_err(), "space should be rejected");
        assert!(
            validate_id(&"a".repeat(65)).is_err(),
            "65-char id should be rejected"
        );
        assert!(validate_id("good-id").is_ok(), "good-id should be accepted");
        assert!(validate_id("ABC_123").is_ok(), "ABC_123 should be accepted");
        assert!(validate_id("a").is_ok(), "single char should be accepted");
        assert!(
            validate_id(&"a".repeat(64)).is_ok(),
            "64-char id should be accepted"
        );
    }

    #[tokio::test]
    async fn update_task_fields() {
        let dir = temp_test_dir("update-task-fields");
        let server = PlansServer::new(dir);

        let add = server
            .handle_add_task(AddTaskParams {
                title: "Before".to_string(),
                plan: "plan-a".to_string(),
                summary: Some("old summary".to_string()),
                author: None,
                assignee: Some("alice".to_string()),
                executor: None,
                tags: vec![],
                status: Some("open".to_string()),
                body: Some("body".to_string()),
                id: None,
                dependencies: vec![],
            })
            .await
            .unwrap();
        let id = extract_id(&extract_text(add));

        server
            .handle_update_task(UpdateTaskParams {
                plan: "plan-a".to_string(),
                id: id.clone(),
                title: Some("After".to_string()),
                summary: Some("new summary".to_string()),
                author: None,
                assignee: Some("bob".to_string()),
                executor: None,
                tags: None,
                status: Some("in_progress".to_string()),
                replace_body: None,
                append_body: None,
                replace_in_body: None,
                dependencies: None,
            })
            .await
            .unwrap();

        let got = server
            .handle_get_task(GetTaskParams {
                plan: "plan-a".to_string(),
                id,
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
        assert_eq!(value["title"], "After");
        assert_eq!(value["status"], "in_progress");
        assert_eq!(value["summary"], "new summary");
        assert_eq!(value["assignee"], "bob");
    }

    #[tokio::test]
    async fn append_body_via_update_task() {
        let dir = temp_test_dir("append-task-body");
        let server = PlansServer::new(dir);

        let add = server
            .handle_add_task(AddTaskParams {
                title: "Append".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: Some("line1".to_string()),
                id: None,
                dependencies: vec![],
            })
            .await
            .unwrap();
        let id = extract_id(&extract_text(add));

        server
            .handle_update_task(UpdateTaskParams {
                plan: "plan-a".to_string(),
                id: id.clone(),
                append_body: Some("line2".to_string()),
                title: None,
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: None,
                status: None,
                replace_body: None,
                replace_in_body: None,
                dependencies: None,
            })
            .await
            .unwrap();

        let got = server
            .handle_get_task(GetTaskParams {
                plan: "plan-a".to_string(),
                id,
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
        assert_eq!(value["body"], "line1\nline2");
    }

    #[tokio::test]
    async fn delete_task() {
        let dir = temp_test_dir("delete-task");
        let server = PlansServer::new(dir);

        let add = server
            .handle_add_task(AddTaskParams {
                title: "Delete me".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: None,
                id: None,
                dependencies: vec![],
            })
            .await
            .unwrap();
        let id = extract_id(&extract_text(add));

        server
            .handle_delete_task(DeleteTaskParams {
                plan: "plan-a".to_string(),
                id: id.clone(),
            })
            .await
            .unwrap();

        let err = server
            .handle_get_task(GetTaskParams {
                plan: "plan-a".to_string(),
                id,
            })
            .await
            .unwrap_err();
        assert!(err.message.contains("not found"));
    }

    #[tokio::test]
    async fn list_tasks_scoped_to_plan() {
        // Tasks are plan-scoped; each plan's list shows only that plan's tasks
        let dir = temp_test_dir("list-tasks-scoped");
        let server = PlansServer::new(dir);

        for plan in ["plan-a", "plan-b"] {
            server
                .handle_add_task(AddTaskParams {
                    title: format!("task for {plan}"),
                    plan: plan.to_string(),
                    summary: None,
                    author: None,
                    assignee: None,
                    executor: None,
                    tags: vec![],
                    status: None,
                    body: None,
                    id: None,
                    dependencies: vec![],
                })
                .await
                .unwrap();
        }

        // Listing plan-a returns only plan-a's task
        let result_a = server
            .handle_list_tasks(ListTasksParams {
                plan: "plan-a".to_string(),
                filter: "all".to_string(),
                tag: None,
            })
            .await
            .unwrap();
        let items_a: Value = serde_json::from_str(&extract_text(result_a)).unwrap();
        assert_eq!(items_a.as_array().unwrap().len(), 1);
        assert_eq!(items_a[0]["plan"], "plan-a");

        // Listing plan-b returns only plan-b's task
        let result_b = server
            .handle_list_tasks(ListTasksParams {
                plan: "plan-b".to_string(),
                filter: "all".to_string(),
                tag: None,
            })
            .await
            .unwrap();
        let items_b: Value = serde_json::from_str(&extract_text(result_b)).unwrap();
        assert_eq!(items_b.as_array().unwrap().len(), 1);
        assert_eq!(items_b[0]["plan"], "plan-b");
    }

    #[tokio::test]
    async fn list_tasks_by_tag() {
        let dir = temp_test_dir("list-tasks-by-tag");
        let server = PlansServer::new(dir);

        // Create task with "urgent" tag
        server
            .handle_add_task(AddTaskParams {
                title: "tagged task".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec!["urgent".to_string()],
                status: None,
                body: None,
                id: None,
                dependencies: vec![],
            })
            .await
            .unwrap();

        // Create task without the tag
        server
            .handle_add_task(AddTaskParams {
                title: "untagged task".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec!["normal".to_string()],
                status: None,
                body: None,
                id: None,
                dependencies: vec![],
            })
            .await
            .unwrap();

        let result = server
            .handle_list_tasks(ListTasksParams {
                filter: "all".to_string(),
                tag: Some("urgent".to_string()),
                plan: "plan-a".to_string(),
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(result)).unwrap();
        let items = value.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["title"], "tagged task");
    }

    #[tokio::test]
    async fn update_task_cross_plan_move() {
        // Tasks are scoped to a plan — update stays within the plan
        let dir = temp_test_dir("update-task-cross-plan");
        let server = PlansServer::new(dir.clone());

        let add = server
            .handle_add_task(AddTaskParams {
                title: "task to update".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: None,
                id: None,
                dependencies: vec![],
            })
            .await
            .unwrap();
        let id = extract_id(&extract_text(add));

        // Update the task status (stays in plan-a)
        server
            .handle_update_task(UpdateTaskParams {
                plan: "plan-a".to_string(),
                id: id.clone(),
                title: None,
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: None,
                status: Some("closed".to_string()),
                replace_body: None,
                append_body: None,
                replace_in_body: None,
                dependencies: None,
            })
            .await
            .unwrap();

        // File should still be in plan-a/tasks/
        assert!(
            dir.join("plan-a/tasks")
                .join(format!("{}.md", normalize_id(&id)))
                .exists(),
            "task file should remain in plan-a"
        );

        // get_task should return the updated status
        let got = server
            .handle_get_task(GetTaskParams {
                plan: "plan-a".to_string(),
                id,
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
        assert_eq!(value["plan"], "plan-a");
        assert_eq!(value["status"], "closed");
    }

    #[tokio::test]
    async fn update_plan_append_content() {
        let dir = temp_test_dir("update-plan-append-content");
        let server = PlansServer::new(dir);

        server
            .handle_add_plan(AddPlanParams {
                name: "plan-a".to_string(),
                title: Some("Plan".to_string()),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                body: Some("line1".to_string()),
            })
            .await
            .unwrap();

        server
            .handle_update_plan(UpdatePlanParams {
                name: "plan-a".to_string(),
                replace_content: None,
                append_content: Some("line2".to_string()),
                replace_in_content: None,
                title: None,
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                tasks: None,
            })
            .await
            .unwrap();

        let got = server
            .handle_get_plan(GetPlanParams {
                name: "plan-a".to_string(),
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
        assert_eq!(
            value["body"],
            "line1
line2"
        );
    }

    #[tokio::test]
    async fn update_plan_replace_in_content() {
        let dir = temp_test_dir("update-plan-replace-in-content");
        let server = PlansServer::new(dir);

        server
            .handle_add_plan(AddPlanParams {
                name: "plan-a".to_string(),
                title: Some("Plan".to_string()),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                body: Some("hello world".to_string()),
            })
            .await
            .unwrap();

        server
            .handle_update_plan(UpdatePlanParams {
                name: "plan-a".to_string(),
                replace_content: None,
                append_content: None,
                replace_in_content: Some(ReplaceInContent {
                    old_text: "world".to_string(),
                    new_text: "there".to_string(),
                    replace_all: None,
                }),
                title: None,
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                tasks: None,
            })
            .await
            .unwrap();

        let got = server
            .handle_get_plan(GetPlanParams {
                name: "plan-a".to_string(),
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
        assert_eq!(value["body"], "hello there");
    }

    #[tokio::test]
    async fn update_plan_replace_in_content_not_found() {
        let dir = temp_test_dir("update-plan-replace-in-content-not-found");
        let server = PlansServer::new(dir);

        server
            .handle_add_plan(AddPlanParams {
                name: "plan-a".to_string(),
                title: Some("Plan".to_string()),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                body: Some("hello world".to_string()),
            })
            .await
            .unwrap();

        let err = server
            .handle_update_plan(UpdatePlanParams {
                name: "plan-a".to_string(),
                replace_content: None,
                append_content: None,
                replace_in_content: Some(ReplaceInContent {
                    old_text: "missing".to_string(),
                    new_text: "there".to_string(),
                    replace_all: None,
                }),
                title: None,
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                tasks: None,
            })
            .await
            .unwrap_err();
        assert!(
            err.message.contains("not found"),
            "expected not found error: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn update_plan_replace_in_content_empty_old_text_error() {
        let dir = temp_test_dir("update-plan-replace-in-empty-old-text");
        let server = PlansServer::new(dir);

        let err = server
            .handle_update_plan(UpdatePlanParams {
                name: "plan-a".to_string(),
                replace_content: None,
                append_content: None,
                replace_in_content: Some(ReplaceInContent {
                    old_text: "".to_string(),
                    new_text: "something".to_string(),
                    replace_all: None,
                }),
                title: None,
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                tasks: None,
            })
            .await
            .unwrap_err();
        assert!(
            err.message.contains("empty"),
            "expected empty old_text error: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn update_plan_two_content_fields_error() {
        let dir = temp_test_dir("update-plan-two-content-fields-error");
        let server = PlansServer::new(dir);

        let err = server
            .handle_update_plan(UpdatePlanParams {
                name: "plan-a".to_string(),
                replace_content: Some("one".to_string()),
                append_content: Some("two".to_string()),
                replace_in_content: None,
                title: None,
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                tasks: None,
            })
            .await
            .unwrap_err();
        assert!(
            err.message.contains("at most one"),
            "expected exclusivity error: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn update_plan_no_content_preserves_body() {
        let dir = temp_test_dir("update-plan-no-content-preserves-body");
        let server = PlansServer::new(dir);

        server
            .handle_add_plan(AddPlanParams {
                name: "plan-a".to_string(),
                title: Some("Plan".to_string()),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                body: Some("keep me".to_string()),
            })
            .await
            .unwrap();

        server
            .handle_update_plan(UpdatePlanParams {
                name: "plan-a".to_string(),
                replace_content: None,
                append_content: None,
                replace_in_content: None,
                title: Some("Renamed".to_string()),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                tasks: None,
            })
            .await
            .unwrap();

        let got = server
            .handle_get_plan(GetPlanParams {
                name: "plan-a".to_string(),
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
        assert_eq!(value["title"], "Renamed");
        assert_eq!(value["body"], "keep me");
    }

    #[tokio::test]
    async fn update_note_fields() {
        let dir = temp_test_dir("update-note-fields");
        let server = PlansServer::new(dir);

        server
            .handle_add_note(AddNoteParams {
                plan: "plan-a".to_string(),
                id: Some("my-note".to_string()),
                body: "hello world".to_string(),
                summary: Some("before".to_string()),
                author: Some("alice".to_string()),
            })
            .await
            .unwrap();

        server
            .handle_update_note(UpdateNoteParams {
                plan: "plan-a".to_string(),
                note_id: "my-note".to_string(),
                summary: Some("after".to_string()),
                author: Some("bob".to_string()),
                replace_body: None,
                append_body: None,
                replace_in_body: Some(ReplaceInContent {
                    old_text: "world".to_string(),
                    new_text: "there".to_string(),
                    replace_all: None,
                }),
            })
            .await
            .unwrap();

        let got = server
            .handle_get_note(GetNoteParams {
                plan: "plan-a".to_string(),
                note_id: "my-note".to_string(),
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
        assert_eq!(value["summary"], "after");
        assert_eq!(value["author"], "bob");
        assert_eq!(value["body"], "hello there");
    }

    #[tokio::test]
    async fn update_note_not_found() {
        let dir = temp_test_dir("update-note-not-found");
        let server = PlansServer::new(dir);

        let err = server
            .handle_update_note(UpdateNoteParams {
                plan: "plan-a".to_string(),
                note_id: "missing".to_string(),
                summary: None,
                author: None,
                replace_body: Some("body".to_string()),
                append_body: None,
                replace_in_body: None,
            })
            .await
            .unwrap_err();
        assert!(
            err.message.contains("not found"),
            "expected not found error: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn update_plan_batch_creates_tasks() {
        let dir = temp_test_dir("update-plan-batch");
        let server = PlansServer::new(dir.clone());

        server
            .handle_update_plan(UpdatePlanParams {
                name: "plan-a".to_string(),
                replace_content: Some("plan body".to_string()),
                append_content: None,
                replace_in_content: None,
                title: None,
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                tasks: Some(vec![
                    TaskSpec {
                        title: "batch task 1".to_string(),
                        id: None,
                        tags: vec![],
                        status: None,
                        body: None,
                        dependencies: vec![],
                        summary: None,
                        author: None,
                        assignee: None,
                        executor: None,
                    },
                    TaskSpec {
                        title: "batch task 2".to_string(),
                        id: None,
                        tags: vec![],
                        status: None,
                        body: None,
                        dependencies: vec![],
                        summary: None,
                        author: None,
                        assignee: None,
                        executor: None,
                    },
                ]),
            })
            .await
            .unwrap();

        // Both tasks should exist in tasks/ dir
        let tasks_dir = dir.join("plan-a/tasks");
        let task_files: Vec<_> = std::fs::read_dir(&tasks_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("md"))
            .collect();
        assert_eq!(task_files.len(), 2);
    }

    #[tokio::test]
    async fn update_plan_batch_rejects_duplicate_id() {
        let dir = temp_test_dir("update-plan-batch-dup-id");
        let server = PlansServer::new(dir);

        // Pre-create a task with id "existing-id"
        server
            .handle_add_task(AddTaskParams {
                title: "existing".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: None,
                id: Some("existing-id".to_string()),
                dependencies: vec![],
            })
            .await
            .unwrap();

        // Try to batch-create a task with the same id — should fail
        let result = server
            .handle_update_plan(UpdatePlanParams {
                name: "plan-a".to_string(),
                replace_content: Some("".to_string()),
                append_content: None,
                replace_in_content: None,
                title: None,
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                tasks: Some(vec![TaskSpec {
                    title: "duplicate id task".to_string(),
                    id: Some("existing-id".to_string()),
                    tags: vec![],
                    status: None,
                    body: None,
                    dependencies: vec![],
                    summary: None,
                    author: None,
                    assignee: None,
                    executor: None,
                }]),
            })
            .await;
        assert!(result.is_err(), "batch with pre-existing id should fail");
    }

    #[tokio::test]
    async fn update_plan_batch_rejects_intra_batch_duplicate_id() {
        let dir = temp_test_dir("update-plan-batch-intra-dup-id");
        let server = PlansServer::new(dir);

        // Two TaskSpecs with the same id in the same batch — should fail
        let result = server
            .handle_update_plan(UpdatePlanParams {
                name: "plan-a".to_string(),
                replace_content: Some("".to_string()),
                append_content: None,
                replace_in_content: None,
                title: None,
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                tasks: Some(vec![
                    TaskSpec {
                        title: "task one".to_string(),
                        id: Some("shared-id".to_string()),
                        tags: vec![],
                        status: None,
                        body: None,
                        dependencies: vec![],
                        summary: None,
                        author: None,
                        assignee: None,
                        executor: None,
                    },
                    TaskSpec {
                        title: "task two".to_string(),
                        id: Some("shared-id".to_string()),
                        tags: vec![],
                        status: None,
                        body: None,
                        dependencies: vec![],
                        summary: None,
                        author: None,
                        assignee: None,
                        executor: None,
                    },
                ]),
            })
            .await;
        assert!(
            result.is_err(),
            "intra-batch duplicate IDs should be rejected"
        );
    }

    #[tokio::test]
    async fn update_plan_batch_creates_tasks_with_ids() {
        let dir = temp_test_dir("update-plan-batch-with-ids");
        let server = PlansServer::new(dir.clone());

        server
            .handle_update_plan(UpdatePlanParams {
                name: "plan-a".to_string(),
                replace_content: Some("plan body".to_string()),
                append_content: None,
                replace_in_content: None,
                title: None,
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                tasks: Some(vec![
                    TaskSpec {
                        title: "first task".to_string(),
                        id: Some("alpha-task".to_string()),
                        tags: vec![],
                        status: None,
                        body: None,
                        dependencies: vec![],
                        summary: None,
                        author: None,
                        assignee: None,
                        executor: None,
                    },
                    TaskSpec {
                        title: "second task".to_string(),
                        id: Some("beta-task".to_string()),
                        tags: vec![],
                        status: None,
                        body: None,
                        dependencies: vec![],
                        summary: None,
                        author: None,
                        assignee: None,
                        executor: None,
                    },
                ]),
            })
            .await
            .unwrap();

        let alpha_path = dir.join("plan-a").join("tasks").join("alpha-task.md");
        let beta_path = dir.join("plan-a").join("tasks").join("beta-task.md");
        assert!(alpha_path.exists(), "alpha-task.md should exist");
        assert!(beta_path.exists(), "beta-task.md should exist");
    }

    #[tokio::test]
    async fn add_note_duplicate_id_error() {
        let dir = temp_test_dir("add-note-dup-id");
        let server = PlansServer::new(dir);

        server
            .handle_add_note(AddNoteParams {
                plan: "plan-a".to_string(),
                id: Some("dup-note-id".to_string()),
                body: "first note".to_string(),
                summary: None,
                author: None,
            })
            .await
            .unwrap();

        let err = server
            .handle_add_note(AddNoteParams {
                plan: "plan-a".to_string(),
                id: Some("dup-note-id".to_string()),
                body: "second note".to_string(),
                summary: None,
                author: None,
            })
            .await
            .unwrap_err();
        assert!(
            err.message.contains("already exists"),
            "expected 'already exists' in: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn list_tasks_by_plan() {
        let dir = temp_test_dir("list-tasks-by-plan");
        let server = PlansServer::new(dir);

        for plan in ["plan-a", "plan-b"] {
            server
                .handle_add_task(AddTaskParams {
                    title: format!("task for {plan}"),
                    plan: plan.to_string(),
                    summary: None,
                    author: None,
                    assignee: None,
                    executor: None,
                    tags: vec![],
                    status: None,
                    body: None,
                    id: None,
                    dependencies: vec![],
                })
                .await
                .unwrap();
        }

        let result = server
            .handle_list_tasks(ListTasksParams {
                filter: "all".to_string(),
                tag: None,
                plan: "plan-a".to_string(),
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(result)).unwrap();
        let items = value.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["plan"], "plan-a");
        assert_eq!(items[0]["title"], "task for plan-a");
    }

    #[tokio::test]
    async fn add_task_creates_tasks_subdir() {
        let dir = temp_test_dir("add-task-creates-tasks-subdir");
        let server = PlansServer::new(dir.clone());

        server
            .handle_add_task(AddTaskParams {
                title: "Task path".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: None,
                id: None,
                dependencies: vec![],
            })
            .await
            .unwrap();

        assert!(dir.join("plan-a").join("tasks").exists());
    }

    #[tokio::test]
    async fn get_task_missing_id() {
        let dir = temp_test_dir("get-task-missing-id");
        let server = PlansServer::new(dir);

        let err = server
            .handle_get_task(GetTaskParams {
                plan: "plan-a".to_string(),
                id: "task-deadbeef".to_string(),
            })
            .await
            .unwrap_err();
        assert!(err.message.contains("not found"));
    }

    #[tokio::test]
    async fn add_and_get_plan() {
        let dir = temp_test_dir("add-and-get-plan");
        let server = PlansServer::new(dir);

        server
            .handle_add_plan(AddPlanParams {
                name: "plan-a".to_string(),
                title: Some("Test Plan".to_string()),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                body: Some("hello".to_string()),
            })
            .await
            .unwrap();

        let got = server
            .handle_get_plan(GetPlanParams {
                name: "plan-a".to_string(),
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
        assert_eq!(value["title"], "Test Plan");
        assert_eq!(value["body"], "hello");
    }

    #[tokio::test]
    async fn add_plan_duplicate_error() {
        let dir = temp_test_dir("add-plan-duplicate-error");
        let server = PlansServer::new(dir);

        server
            .handle_add_plan(AddPlanParams {
                name: "plan-a".to_string(),
                title: Some("Test Plan".to_string()),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                body: Some("hello".to_string()),
            })
            .await
            .unwrap();

        let err = server
            .handle_add_plan(AddPlanParams {
                name: "plan-a".to_string(),
                title: Some("Test Plan".to_string()),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                body: Some("hello again".to_string()),
            })
            .await
            .unwrap_err();
        assert!(err.message.contains("already exists"));
    }

    #[tokio::test]
    async fn update_plan_creates_if_missing() {
        let dir = temp_test_dir("update-plan-creates-if-missing");
        let server = PlansServer::new(dir.clone());

        server
            .handle_update_plan(UpdatePlanParams {
                name: "plan-a".to_string(),
                title: Some("Created".to_string()),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                replace_content: Some("new body".to_string()),
                append_content: None,
                replace_in_content: None,
                tasks: None,
            })
            .await
            .unwrap();

        assert!(dir.join("plan-a").join("plan.md").exists());
        let got = server
            .handle_get_plan(GetPlanParams {
                name: "plan-a".to_string(),
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
        assert_eq!(value["title"], "Created");
        assert_eq!(value["body"], "new body");
    }

    #[tokio::test]
    async fn update_plan_preserves_metadata() {
        let dir = temp_test_dir("update-plan-preserves-metadata");
        let server = PlansServer::new(dir);

        server
            .handle_add_plan(AddPlanParams {
                name: "plan-a".to_string(),
                title: Some("Test Plan".to_string()),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                body: Some("before".to_string()),
            })
            .await
            .unwrap();

        server
            .handle_update_plan(UpdatePlanParams {
                name: "plan-a".to_string(),
                title: None,
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                replace_content: Some("after".to_string()),
                append_content: None,
                replace_in_content: None,
                tasks: None,
            })
            .await
            .unwrap();

        let got = server
            .handle_get_plan(GetPlanParams {
                name: "plan-a".to_string(),
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
        assert_eq!(value["title"], "Test Plan");
        assert_eq!(value["body"], "after");
    }

    #[tokio::test]
    async fn delete_plan() {
        let dir = temp_test_dir("delete-plan");
        let server = PlansServer::new(dir.clone());

        server
            .handle_add_plan(AddPlanParams {
                name: "plan-a".to_string(),
                title: Some("Test Plan".to_string()),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                body: Some("hello".to_string()),
            })
            .await
            .unwrap();

        server
            .handle_delete_plan(DeletePlanParams {
                name: "plan-a".to_string(),
            })
            .await
            .unwrap();

        assert!(!dir.join("plan-a").exists());
    }

    #[tokio::test]
    async fn list_plans_returns_counts() {
        let dir = temp_test_dir("list-plans-returns-counts");
        let server = PlansServer::new(dir);

        server
            .handle_add_plan(AddPlanParams {
                name: "plan-a".to_string(),
                title: Some("Test Plan".to_string()),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                body: Some("hello".to_string()),
            })
            .await
            .unwrap();
        for idx in 0..2 {
            server
                .handle_add_task(AddTaskParams {
                    title: format!("Task {idx}"),
                    plan: "plan-a".to_string(),
                    summary: None,
                    author: None,
                    assignee: None,
                    executor: None,
                    tags: vec![],
                    status: None,
                    body: None,
                    id: None,
                    dependencies: vec![],
                })
                .await
                .unwrap();
        }
        server
            .handle_add_note(AddNoteParams {
                plan: "plan-a".to_string(),
                id: None,
                body: "note body".to_string(),
                summary: None,
                author: None,
            })
            .await
            .unwrap();

        let result = server.handle_list_plans().await.unwrap();
        let value: Value = serde_json::from_str(&extract_text(result)).unwrap();
        let items = value.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["task_count"], 2);
        assert_eq!(items[0]["note_count"], 1);
    }

    #[tokio::test]
    async fn add_and_get_note() {
        let dir = temp_test_dir("add-and-get-note");
        let server = PlansServer::new(dir);

        let add = server
            .handle_add_note(AddNoteParams {
                plan: "plan-a".to_string(),
                id: None,
                body: "note body".to_string(),
                summary: Some("sum".to_string()),
                author: Some("author".to_string()),
            })
            .await
            .unwrap();
        let id = extract_id(&extract_text(add));

        let got = server
            .handle_get_note(GetNoteParams {
                plan: "plan-a".to_string(),
                note_id: id,
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
        assert_eq!(value["summary"], "sum");
        assert_eq!(value["body"], "note body");
    }

    #[tokio::test]
    async fn delete_note() {
        let dir = temp_test_dir("delete-note");
        let server = PlansServer::new(dir);

        let add = server
            .handle_add_note(AddNoteParams {
                plan: "plan-a".to_string(),
                id: None,
                body: "note body".to_string(),
                summary: None,
                author: None,
            })
            .await
            .unwrap();
        let id = extract_id(&extract_text(add));

        server
            .handle_delete_note(DeleteNoteParams {
                plan: "plan-a".to_string(),
                note_id: id.clone(),
            })
            .await
            .unwrap();

        let err = server
            .handle_get_note(GetNoteParams {
                plan: "plan-a".to_string(),
                note_id: id,
            })
            .await
            .unwrap_err();
        assert!(err.message.contains("not found"));
    }

    #[tokio::test]
    async fn list_notes() {
        let dir = temp_test_dir("list-notes");
        let server = PlansServer::new(dir);

        for idx in 0..2 {
            server
                .handle_add_note(AddNoteParams {
                    plan: "plan-a".to_string(),
                    id: None,
                    body: format!("note {idx}"),
                    summary: Some(format!("summary {idx}")),
                    author: None,
                })
                .await
                .unwrap();
        }

        let result = server
            .handle_list_notes(ListNotesParams {
                plan: "plan-a".to_string(),
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(result)).unwrap();
        let items = value.as_array().unwrap();
        assert_eq!(items.len(), 2);
        let summaries = items
            .iter()
            .map(|item| item["summary"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(summaries.contains(&"summary 0"));
        assert!(summaries.contains(&"summary 1"));
    }

    #[tokio::test]
    async fn add_note_creates_notes_subdir() {
        let dir = temp_test_dir("add-note-creates-notes-subdir");
        let server = PlansServer::new(dir.clone());

        server
            .handle_add_note(AddNoteParams {
                plan: "plan-a".to_string(),
                id: None,
                body: "note body".to_string(),
                summary: None,
                author: None,
            })
            .await
            .unwrap();

        assert!(dir.join("plan-a").join("notes").exists());
    }

    #[tokio::test]
    async fn get_note_returns_frontmatter() {
        let dir = temp_test_dir("get-note-returns-frontmatter");
        let server = PlansServer::new(dir);

        let add = server
            .handle_add_note(AddNoteParams {
                plan: "plan-a".to_string(),
                id: None,
                body: "note body".to_string(),
                summary: Some("test summary".to_string()),
                author: Some("author".to_string()),
            })
            .await
            .unwrap();
        let id = extract_id(&extract_text(add));

        let got = server
            .handle_get_note(GetNoteParams {
                plan: "plan-a".to_string(),
                note_id: id,
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
        assert_eq!(value["summary"], "test summary");
    }

    #[tokio::test]
    async fn get_plan_legacy_raw_markdown() {
        let dir = temp_test_dir("get-plan-legacy-raw-markdown");
        let server = PlansServer::new(dir.clone());

        let plan_dir = dir.join("plan-a");
        fs::create_dir_all(&plan_dir).unwrap();
        fs::write(plan_dir.join("plan.md"), "# Legacy Plan\n\nbody text").unwrap();

        let got = server
            .handle_get_plan(GetPlanParams {
                name: "plan-a".to_string(),
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
        assert_eq!(value["id"], "plan-a");
        assert_eq!(value["body"], "# Legacy Plan\n\nbody text");
    }

    #[tokio::test]
    async fn normalize_note_id_prefix() {
        let dir = temp_test_dir("normalize-note-id-prefix");
        let server = PlansServer::new(dir);

        let add = server
            .handle_add_note(AddNoteParams {
                plan: "plan-a".to_string(),
                id: None,
                body: "note body".to_string(),
                summary: None,
                author: None,
            })
            .await
            .unwrap();
        let id = normalize_id(&extract_id(&extract_text(add)));

        let got = server
            .handle_get_note(GetNoteParams {
                plan: "plan-a".to_string(),
                note_id: format!("note-{id}"),
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
        assert_eq!(value["id"], id);
    }

    #[tokio::test]
    async fn list_tasks_filter() {
        let dir = temp_test_dir("list-tasks-filter");
        let server = PlansServer::new(dir);

        server
            .handle_add_task(AddTaskParams {
                title: "Open".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: Some("open".to_string()),
                body: None,
                id: None,
                dependencies: vec![],
            })
            .await
            .unwrap();
        server
            .handle_add_task(AddTaskParams {
                title: "Closed".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: Some("closed".to_string()),
                body: None,
                id: None,
                dependencies: vec![],
            })
            .await
            .unwrap();

        let result = server
            .handle_list_tasks(ListTasksParams {
                filter: "open".to_string(),
                tag: None,
                plan: "plan-a".to_string(),
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(result)).unwrap();
        assert_eq!(value.as_array().unwrap().len(), 1);
        assert_eq!(value[0]["title"], "Open");
    }

    #[tokio::test]
    async fn task_file_in_tasks_subdir() {
        let dir = temp_test_dir("task-file-in-subdir");
        let server = PlansServer::new(dir.clone());

        let add = server
            .handle_add_task(AddTaskParams {
                title: "Task path".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: None,
                id: None,
                dependencies: vec![],
            })
            .await
            .unwrap();
        let id = extract_id(&extract_text(add));
        let path = dir
            .join("plan-a")
            .join("tasks")
            .join(format!("{}.md", normalize_id(&id)));
        assert!(path.exists());
    }

    #[tokio::test]
    async fn get_task_wrong_plan_fails() {
        let dir = temp_test_dir("get-task-wrong-plan");
        let server = PlansServer::new(dir);

        server
            .handle_add_task(AddTaskParams {
                title: "task".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                id: Some("my-task".to_string()),
                body: None,
                dependencies: vec![],
            })
            .await
            .unwrap();

        let err = server
            .handle_get_task(GetTaskParams {
                plan: "plan-b".to_string(),
                id: "my-task".to_string(),
            })
            .await
            .unwrap_err();
        assert!(
            err.message.contains("not found"),
            "expected 'not found' in: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn update_task_wrong_plan_fails() {
        let dir = temp_test_dir("update-task-wrong-plan");
        let server = PlansServer::new(dir);

        server
            .handle_add_task(AddTaskParams {
                title: "task".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                id: Some("my-task".to_string()),
                body: None,
                dependencies: vec![],
            })
            .await
            .unwrap();

        let err = server
            .handle_update_task(UpdateTaskParams {
                plan: "plan-b".to_string(),
                id: "my-task".to_string(),
                title: Some("new title".to_string()),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: None,
                status: None,
                replace_body: None,
                append_body: None,
                replace_in_body: None,
                dependencies: None,
            })
            .await
            .unwrap_err();
        assert!(
            err.message.contains("not found"),
            "expected 'not found' in: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn append_body_wrong_plan_fails() {
        let dir = temp_test_dir("append-task-wrong-plan");
        let server = PlansServer::new(dir);

        server
            .handle_add_task(AddTaskParams {
                title: "task".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: Some("body".to_string()),
                id: Some("my-task".to_string()),
                dependencies: vec![],
            })
            .await
            .unwrap();

        let err = server
            .handle_update_task(UpdateTaskParams {
                plan: "plan-b".to_string(),
                id: "my-task".to_string(),
                title: None,
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: None,
                status: None,
                replace_body: None,
                append_body: Some("appended".to_string()),
                replace_in_body: None,
                dependencies: None,
            })
            .await
            .unwrap_err();
        assert!(
            err.message.contains("not found"),
            "expected 'not found' in: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn delete_task_wrong_plan_fails() {
        let dir = temp_test_dir("delete-task-wrong-plan");
        let server = PlansServer::new(dir);

        server
            .handle_add_task(AddTaskParams {
                title: "task".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                id: Some("my-task".to_string()),
                body: None,
                dependencies: vec![],
            })
            .await
            .unwrap();

        let err = server
            .handle_delete_task(DeleteTaskParams {
                plan: "plan-b".to_string(),
                id: "my-task".to_string(),
            })
            .await
            .unwrap_err();
        assert!(
            err.message.contains("not found"),
            "expected 'not found' in: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn delete_note_wrong_plan_fails() {
        let dir = temp_test_dir("delete-note-wrong-plan");
        let server = PlansServer::new(dir);

        server
            .handle_add_note(AddNoteParams {
                plan: "plan-a".to_string(),
                id: Some("my-note".to_string()),
                body: "body".to_string(),
                summary: None,
                author: None,
            })
            .await
            .unwrap();

        let err = server
            .handle_delete_note(DeleteNoteParams {
                plan: "plan-b".to_string(),
                note_id: "my-note".to_string(),
            })
            .await
            .unwrap_err();
        assert!(
            err.message.contains("not found"),
            "expected 'not found' in: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn cleanup_deletes_stale_plan_but_keeps_fresh_plan() {
        let dir = temp_test_dir("cleanup-stale-plan");
        let stale_plan = dir.join("stale-plan");
        let fresh_plan = dir.join("fresh-plan");

        fs::create_dir_all(&stale_plan).unwrap();
        fs::write(stale_plan.join("plan.md"), "stale").unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        fs::create_dir_all(&fresh_plan).unwrap();
        fs::write(fresh_plan.join("plan.md"), "fresh").unwrap();

        run_cleanup_pass(&dir, Duration::from_millis(10)).await;

        assert!(!stale_plan.exists(), "stale plan should be deleted");
        assert!(fresh_plan.exists(), "fresh plan should be kept");
    }

    #[test]
    fn validate_plan_name_rejects_invalid() {
        assert!(validate_plan_name("").is_err(), "empty string rejected");
        assert!(validate_plan_name("   ").is_err(), "whitespace rejected");
        assert!(validate_plan_name("a/b").is_err(), "slash rejected");
        assert!(validate_plan_name("../etc").is_err(), "traversal rejected");
        assert!(
            validate_plan_name("my-plan").is_ok(),
            "normal name accepted"
        );
        assert!(
            validate_plan_name("plan a").is_ok(),
            "space normalized to hyphen"
        );
    }
}

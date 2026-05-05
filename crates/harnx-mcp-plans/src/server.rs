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
use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
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
    #[serde(default = "default_open_status")]
    filter: String,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    plan: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct GetTaskParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    plan: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct AddTaskParams {
    title: String,
    plan: String,
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
    key: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct UpdateTaskParams {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    plan: Option<String>,
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
    body: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    dependencies: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct AppendTaskParams {
    id: String,
    text: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct DeleteTaskParams {
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
    content: String,
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
struct TaskSpec {
    title: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    key: Option<String>,
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
        let status_filter = match params.filter.as_str() {
            "all" => None,
            other => Some(other),
        };
        let tasks = list_tasks(
            &self.dir,
            params.plan.as_deref(),
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
        let task = if let Some(id) = params.id {
            read_task_by_id(&self.dir, &id).map_err(|e| ErrorData::invalid_params(e, None))?
        } else if let (Some(key), Some(plan)) = (params.key, params.plan) {
            let plan_name = normalize_plan_name(&plan);
            let tasks = list_tasks(&self.dir, Some(&plan_name), None, None);
            tasks
                .into_iter()
                .find(|task| task.front.key.as_deref() == Some(key.as_str()))
                .ok_or_else(|| {
                    ErrorData::invalid_params(
                        format!("task not found for key '{}' in plan '{}'", key, plan_name),
                        None,
                    )
                })?
        } else {
            return Err(ErrorData::invalid_params(
                "provide either 'id' or both 'key' and 'plan'".to_string(),
                None,
            ));
        };

        result_json(
            serde_json::to_value(TaskWithBody {
                front: &task.front,
                body: &task.body,
            })
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?,
        )
    }

    async fn handle_add_task(&self, params: AddTaskParams) -> Result<CallToolResult, ErrorData> {
        let plan_name = normalize_plan_name(&params.plan);
        let plan_path = plan_dir(&self.dir, &plan_name);
        if !plan_path.exists() {
            std::fs::create_dir_all(&plan_path)
                .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        }

        if let Some(key) = params.key.as_ref() {
            let duplicate = list_tasks(&self.dir, Some(&plan_name), None, None)
                .into_iter()
                .any(|task| task.front.key.as_deref() == Some(key.as_str()));
            if duplicate {
                return Err(ErrorData::invalid_params(
                    format!("key '{}' already exists in plan '{}'", key, plan_name),
                    None,
                ));
            }
        }

        let id = gen_id();
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
                key: params.key,
                dependencies: params.dependencies,
            },
            body: params.body.unwrap_or_default(),
        };
        write_task(&self.dir, &task).map_err(|err| ErrorData::internal_error(err, None))?;
        result_text(format!(
            "added task {} to plan {}",
            display_id(&task.front.id),
            task.front.plan
        ))
    }

    async fn handle_update_task(
        &self,
        params: UpdateTaskParams,
    ) -> Result<CallToolResult, ErrorData> {
        let (current_plan, current_path) = find_task_file(&self.dir, &params.id)
            .map_err(|err| ErrorData::invalid_params(err, None))?;
        let mut task = read_task(&self.dir, &current_plan, &params.id)
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
        if let Some(body) = params.body {
            task.body = body;
        }
        if let Some(key) = params.key {
            task.front.key = Some(key);
        }
        if let Some(dependencies) = params.dependencies {
            task.front.dependencies = dependencies;
        }

        let new_plan = params
            .plan
            .as_deref()
            .map(normalize_plan_name)
            .unwrap_or_else(|| task.front.plan.clone());
        task.front.plan = new_plan;

        if let Some(key) = task.front.key.as_ref() {
            let duplicate = list_tasks(&self.dir, Some(&task.front.plan), None, None)
                .into_iter()
                .any(|existing| {
                    existing.front.key.as_deref() == Some(key.as_str())
                        && existing.front.id != task.front.id
                });
            if duplicate {
                return Err(ErrorData::invalid_params(
                    format!("key '{}' already exists in plan '{}'", key, task.front.plan),
                    None,
                ));
            }
        }

        task.front.updated_at = Some(now_iso());

        write_task(&self.dir, &task).map_err(|err| ErrorData::internal_error(err, None))?;
        if current_path.exists() && current_plan != task.front.plan {
            std::fs::remove_file(&current_path)
                .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        }
        result_text(format!("updated task {}", display_id(&task.front.id)))
    }

    async fn handle_append_task(
        &self,
        params: AppendTaskParams,
    ) -> Result<CallToolResult, ErrorData> {
        let mut task = read_task_by_id(&self.dir, &params.id)
            .map_err(|err| ErrorData::invalid_params(err, None))?;
        if !task.body.is_empty() && !task.body.ends_with('\n') {
            task.body.push('\n');
        }
        task.body.push_str(&params.text);
        task.front.updated_at = Some(now_iso());
        write_task(&self.dir, &task).map_err(|err| ErrorData::internal_error(err, None))?;
        result_text(format!("appended to task {}", display_id(&task.front.id)))
    }

    async fn handle_delete_task(
        &self,
        params: DeleteTaskParams,
    ) -> Result<CallToolResult, ErrorData> {
        let (_, path) = find_task_file(&self.dir, &params.id)
            .map_err(|err| ErrorData::invalid_params(err, None))?;
        std::fs::remove_file(&path)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        result_text(format!("deleted task {}", display_id(&params.id)))
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
        let name = normalize_plan_name(&params.name);
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
        result_text(format!("added plan {}", name))
    }

    async fn handle_get_plan(&self, params: GetPlanParams) -> Result<CallToolResult, ErrorData> {
        let name = normalize_plan_name(&params.name);
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
        let name = normalize_plan_name(&params.name);
        let dir = plan_dir(&self.dir, &name);
        std::fs::create_dir_all(&dir)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;

        let path = plan_file_path(&self.dir, &name);
        let existing = if path.exists() {
            let content = std::fs::read_to_string(&path)
                .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
            let (front, _) = parse_plan_frontmatter(&content, &name)
                .map_err(|err| ErrorData::internal_error(err, None))?;
            front
        } else {
            PlanFrontMatter {
                id: name.clone(),
                created_at: now_iso(),
                ..Default::default()
            }
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
            body: params.content,
        };

        // Validate task key uniqueness BEFORE writing anything
        let task_specs = params.tasks;
        if let Some(ref tasks) = task_specs {
            let existing_tasks = list_tasks(&self.dir, Some(&name), None, None);
            let mut seen_keys: Vec<String> = existing_tasks
                .iter()
                .filter_map(|t| t.front.key.clone())
                .collect();
            for spec in tasks {
                if let Some(ref key) = spec.key {
                    if seen_keys.iter().any(|k| k == key) {
                        return Err(ErrorData::invalid_params(
                            format!("key '{}' already exists in plan '{}'", key, name),
                            None,
                        ));
                    }
                    seen_keys.push(key.clone());
                }
            }
        }

        // Build task records before writing anything (all validation already done above)
        let mut task_records = Vec::new();
        if let Some(tasks) = task_specs {
            for spec in tasks {
                let id = gen_id();
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
                        key: spec.key,
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

        let text = if created_task_ids.is_empty() {
            format!("updated plan {}", name)
        } else {
            format!(
                "updated plan {} and added tasks {}",
                name,
                created_task_ids.join(", ")
            )
        };
        result_text(text)
    }

    async fn handle_delete_plan(
        &self,
        params: DeletePlanParams,
    ) -> Result<CallToolResult, ErrorData> {
        let name = normalize_plan_name(&params.name);
        let dir = plan_dir(&self.dir, &name);
        if !dir.exists() {
            return Err(ErrorData::invalid_params(
                format!("plan '{}' not found", name),
                None,
            ));
        }
        std::fs::remove_dir_all(&dir)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        result_text(format!("deleted plan {}", name))
    }

    async fn handle_list_notes(
        &self,
        params: ListNotesParams,
    ) -> Result<CallToolResult, ErrorData> {
        let plan = normalize_plan_name(&params.plan);
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
        let plan = normalize_plan_name(&params.plan);
        std::fs::create_dir_all(notes_dir(&self.dir, &plan))
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        let note = NoteRecord {
            front: NoteFrontMatter {
                id: gen_id(),
                summary: params.summary,
                author: params.author,
                created_at: now_iso(),
                updated_at: None,
            },
            body: params.body,
        };
        write_note(&self.dir, &plan, &note).map_err(|err| ErrorData::internal_error(err, None))?;
        result_text(format!(
            "added note {} to plan {}",
            display_note_id(&note.front.id),
            plan
        ))
    }

    async fn handle_get_note(&self, params: GetNoteParams) -> Result<CallToolResult, ErrorData> {
        let plan = normalize_plan_name(&params.plan);
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
        let plan = normalize_plan_name(&params.plan);
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
        std::fs::remove_file(&path)
            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
        result_text(format!("deleted note {}", display_note_id(&params.note_id)))
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
    ListTasksParams,
    "ListTasksParams",
    |gen: &mut SchemaGenerator| vec![
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
        (
            "plan",
            "Optional plan filter; if omitted, list across all plans",
            gen.subschema_for::<Option<String>>()
        ),
    ],
    &[]
);

impl_json_schema!(
    GetTaskParams,
    "GetTaskParams",
    |gen: &mut SchemaGenerator| vec![
        ("id", "Task ID", gen.subschema_for::<Option<String>>()),
        (
            "key",
            "Task key within plan; requires plan when id omitted",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "plan",
            "Plan name for key lookup",
            gen.subschema_for::<Option<String>>()
        ),
    ],
    &[]
);

impl_json_schema!(
    AddTaskParams,
    "AddTaskParams",
    |gen: &mut SchemaGenerator| vec![
        ("title", "Short title", gen.subschema_for::<String>()),
        ("plan", "Plan name", gen.subschema_for::<String>()),
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
            "key",
            "Optional unique key within plan",
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
    |gen: &mut SchemaGenerator| vec![
        ("id", "Task ID", gen.subschema_for::<String>()),
        (
            "title",
            "Optional title",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "plan",
            "Optional plan name",
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
            "body",
            "Replace markdown body",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "key",
            "Optional task key",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "dependencies",
            "Replace dependencies with provided list",
            gen.subschema_for::<Option<Vec<String>>>()
        ),
    ],
    &["id"]
);

impl_json_schema!(
    AppendTaskParams,
    "AppendTaskParams",
    |gen: &mut SchemaGenerator| vec![
        ("id", "Task ID", gen.subschema_for::<String>()),
        ("text", "Text to append", gen.subschema_for::<String>()),
    ],
    &["id", "text"]
);

impl_json_schema!(
    DeleteTaskParams,
    "DeleteTaskParams",
    |gen: &mut SchemaGenerator| vec![("id", "Task ID", gen.subschema_for::<String>()),],
    &["id"]
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
    |gen: &mut SchemaGenerator| vec![
        ("name", "Plan name", gen.subschema_for::<String>()),
        (
            "content",
            "Plan markdown body",
            gen.subschema_for::<String>()
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
    ],
    &["name", "content"]
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
    TaskSpec,
    "TaskSpec",
    |gen: &mut SchemaGenerator| vec![
        ("title", "Task title", gen.subschema_for::<String>()),
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
        ("key", "Task key", gen.subschema_for::<Option<String>>()),
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
    let content = std::fs::read_to_string(task_file_path(dir, plan_name, id))
        .map_err(|err| err.to_string())?;
    let (front, body) = parse_task_frontmatter(&content)?;
    Ok(TaskRecord { front, body })
}

fn find_task_file(dir: &Path, id: &str) -> Result<(String, PathBuf), String> {
    let normalized = normalize_id(id);
    for plan in plan_dirs(dir) {
        let Some(plan_name) = plan.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        let path = tasks_dir(dir, plan_name).join(format!("{}.md", normalized));
        if path.exists() {
            return Ok((plan_name.to_string(), path));
        }
    }
    Err(format!("task {} not found", display_id(id)))
}

fn read_task_by_id(dir: &Path, id: &str) -> Result<TaskRecord, String> {
    let (plan, _) = find_task_file(dir, id)?;
    read_task(dir, &plan, id)
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
                    .with_meta(Meta(json!({"call_template": "plans", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("add_plan", "Create a new plan with optional metadata.", Map::new())
                    .with_input_schema::<AddPlanParams>()
                    .with_meta(Meta(json!({"call_template": "+ plan {{ args.name }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("get_plan", "Read plan metadata, body, and list task/note IDs.", Map::new())
                    .with_input_schema::<GetPlanParams>()
                    .with_meta(Meta(json!({"call_template": "plan {{ args.name }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("update_plan", "Update plan body and metadata. Creates plan if it doesn't exist. Optionally batch-create tasks.", Map::new())
                    .with_input_schema::<UpdatePlanParams>()
                    .with_meta(Meta(json!({"call_template": "~ plan {{ args.name }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("delete_plan", "Delete an entire plan and all its tasks and notes.", Map::new())
                    .with_input_schema::<DeletePlanParams>()
                    .with_meta(Meta(json!({"call_template": "- plan {{ args.name }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("list_tasks", "List tasks across plans with optional filters.", Map::new())
                    .with_input_schema::<ListTasksParams>()
                    .with_meta(Meta(json!({"call_template": "tasks", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("add_task", "Create a task in a plan.", Map::new())
                    .with_input_schema::<AddTaskParams>()
                    .with_meta(Meta(json!({"call_template": "+ task {{ args.title }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("get_task", "Read a task by ID or by key within a plan.", Map::new())
                    .with_input_schema::<GetTaskParams>()
                    .with_meta(Meta(json!({"call_template": "task {{ args.id | default(args.key) }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("update_task", "Update a task and optionally move it to another plan.", Map::new())
                    .with_input_schema::<UpdateTaskParams>()
                    .with_meta(Meta(json!({"call_template": "~ task {{ args.id }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("append_task", "Append markdown text to task body.", Map::new())
                    .with_input_schema::<AppendTaskParams>()
                    .with_meta(Meta(json!({"call_template": ">> task {{ args.id }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("delete_task", "Delete a task by ID.", Map::new())
                    .with_input_schema::<DeleteTaskParams>()
                    .with_meta(Meta(json!({"call_template": "- task {{ args.id }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("list_notes", "List notes for a plan.", Map::new())
                    .with_input_schema::<ListNotesParams>()
                    .with_meta(Meta(json!({"call_template": "notes {{ args.plan }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("add_note", "Add a note to a plan.", Map::new())
                    .with_input_schema::<AddNoteParams>()
                    .with_meta(Meta(json!({"call_template": "+ note {{ args.plan }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("get_note", "Read a note from a plan.", Map::new())
                    .with_input_schema::<GetNoteParams>()
                    .with_meta(Meta(json!({"call_template": "note {{ args.note_id }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
                Tool::new("delete_note", "Delete a note from a plan.", Map::new())
                    .with_input_schema::<DeleteNoteParams>()
                    .with_meta(Meta(json!({"call_template": "- note {{ args.note_id }}", "result_template": "{{ result.content[0].text | default('') }}"}).as_object().unwrap().clone())),
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
            "append_task" => {
                let params = parse_arguments::<AppendTaskParams>(request.arguments)?;
                self.handle_append_task(params).await
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
                key: None,
                dependencies: vec![],
            })
            .await
            .unwrap();
        let id = extract_id(&extract_text(add));

        let got = server
            .handle_get_task(GetTaskParams {
                id: Some(id),
                key: None,
                plan: None,
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
        assert_eq!(value["title"], "Task 1");
        assert_eq!(value["summary"], "sum");
        assert_eq!(value["body"], "body");
    }

    #[tokio::test]
    async fn get_task_by_key() {
        let dir = temp_test_dir("get-task-by-key");
        let server = PlansServer::new(dir);

        server
            .handle_add_task(AddTaskParams {
                title: "Task key".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: None,
                key: Some("ABC-1".to_string()),
                dependencies: vec![],
            })
            .await
            .unwrap();

        let got = server
            .handle_get_task(GetTaskParams {
                id: None,
                key: Some("ABC-1".to_string()),
                plan: Some("plan-a".to_string()),
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
        assert_eq!(value["key"], "ABC-1");
        assert_eq!(value["title"], "Task key");
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
                key: None,
                dependencies: vec![],
            })
            .await
            .unwrap();
        let id = extract_id(&extract_text(add));

        server
            .handle_update_task(UpdateTaskParams {
                id: id.clone(),
                title: Some("After".to_string()),
                plan: None,
                summary: Some("new summary".to_string()),
                author: None,
                assignee: Some("bob".to_string()),
                executor: None,
                tags: None,
                status: Some("in_progress".to_string()),
                body: None,
                key: None,
                dependencies: None,
            })
            .await
            .unwrap();

        let got = server
            .handle_get_task(GetTaskParams {
                id: Some(id),
                key: None,
                plan: None,
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
    async fn append_task_body() {
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
                key: None,
                dependencies: vec![],
            })
            .await
            .unwrap();
        let id = extract_id(&extract_text(add));

        server
            .handle_append_task(AppendTaskParams {
                id: id.clone(),
                text: "line2".to_string(),
            })
            .await
            .unwrap();

        let got = server
            .handle_get_task(GetTaskParams {
                id: Some(id),
                key: None,
                plan: None,
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
                key: None,
                dependencies: vec![],
            })
            .await
            .unwrap();
        let id = extract_id(&extract_text(add));

        server
            .handle_delete_task(DeleteTaskParams { id: id.clone() })
            .await
            .unwrap();

        let err = server
            .handle_get_task(GetTaskParams {
                id: Some(id),
                key: None,
                plan: None,
            })
            .await
            .unwrap_err();
        assert!(err.message.contains("not found"));
    }

    #[tokio::test]
    async fn list_tasks_across_plans() {
        let dir = temp_test_dir("list-tasks-across-plans");
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
                    key: None,
                    dependencies: vec![],
                })
                .await
                .unwrap();
        }

        let result = server
            .handle_list_tasks(ListTasksParams {
                filter: "all".to_string(),
                tag: None,
                plan: None,
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(result)).unwrap();
        let items = value.as_array().unwrap();
        assert_eq!(items.len(), 2);
        let plans = items
            .iter()
            .map(|item| item["plan"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(plans.contains(&"plan-a"));
        assert!(plans.contains(&"plan-b"));
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
                key: None,
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
                key: None,
                dependencies: vec![],
            })
            .await
            .unwrap();

        let result = server
            .handle_list_tasks(ListTasksParams {
                filter: "all".to_string(),
                tag: Some("urgent".to_string()),
                plan: None,
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
        let dir = temp_test_dir("update-task-cross-plan");
        let server = PlansServer::new(dir.clone());

        let add = server
            .handle_add_task(AddTaskParams {
                title: "movable task".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: None,
                key: None,
                dependencies: vec![],
            })
            .await
            .unwrap();
        let id = extract_id(&extract_text(add));

        // Move to plan-b
        server
            .handle_update_task(UpdateTaskParams {
                id: id.clone(),
                title: None,
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: None,
                plan: Some("plan-b".to_string()),
                status: None,
                body: None,
                key: None,
                dependencies: None,
            })
            .await
            .unwrap();

        // File should be in plan-b/tasks/, not plan-a/tasks/
        assert!(
            !dir.join("plan-a/tasks")
                .join(format!("{}.md", normalize_id(&id)))
                .exists(),
            "old task file should be deleted"
        );
        assert!(
            dir.join("plan-b/tasks")
                .join(format!("{}.md", normalize_id(&id)))
                .exists(),
            "task should exist in new plan"
        );

        // get_task should return plan-b
        let got = server
            .handle_get_task(GetTaskParams {
                id: Some(id),
                key: None,
                plan: None,
            })
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&extract_text(got)).unwrap();
        assert_eq!(value["plan"], "plan-b");
    }

    #[tokio::test]
    async fn update_task_duplicate_key_rejected() {
        let dir = temp_test_dir("update-task-dup-key");
        let server = PlansServer::new(dir);

        // Create two tasks in same plan
        let add1 = server
            .handle_add_task(AddTaskParams {
                title: "task one".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: None,
                key: Some("key-one".to_string()),
                dependencies: vec![],
            })
            .await
            .unwrap();
        let id1 = extract_id(&extract_text(add1));

        server
            .handle_add_task(AddTaskParams {
                title: "task two".to_string(),
                plan: "plan-a".to_string(),
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: vec![],
                status: None,
                body: None,
                key: Some("key-two".to_string()),
                dependencies: vec![],
            })
            .await
            .unwrap();

        // Try to update task-one's key to "key-two" — should fail
        let result = server
            .handle_update_task(UpdateTaskParams {
                id: id1,
                title: None,
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                tags: None,
                plan: None,
                status: None,
                body: None,
                key: Some("key-two".to_string()),
                dependencies: None,
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn update_plan_batch_creates_tasks() {
        let dir = temp_test_dir("update-plan-batch");
        let server = PlansServer::new(dir.clone());

        server
            .handle_update_plan(UpdatePlanParams {
                name: "plan-a".to_string(),
                content: "plan body".to_string(),
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
                        tags: vec![],
                        status: None,
                        body: None,
                        key: Some("bt1".to_string()),
                        dependencies: vec![],
                        summary: None,
                        author: None,
                        assignee: None,
                        executor: None,
                    },
                    TaskSpec {
                        title: "batch task 2".to_string(),
                        tags: vec![],
                        status: None,
                        body: None,
                        key: Some("bt2".to_string()),
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
    async fn update_plan_batch_rejects_duplicate_key() {
        let dir = temp_test_dir("update-plan-batch-dup-key");
        let server = PlansServer::new(dir);

        // Pre-create a task with key "existing-key"
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
                key: Some("existing-key".to_string()),
                dependencies: vec![],
            })
            .await
            .unwrap();

        // Try to batch-create a task with the same key — should fail
        let result = server
            .handle_update_plan(UpdatePlanParams {
                name: "plan-a".to_string(),
                content: "".to_string(),
                title: None,
                summary: None,
                author: None,
                assignee: None,
                executor: None,
                git_branch: None,
                github_owner_repo: None,
                tasks: Some(vec![TaskSpec {
                    title: "duplicate key task".to_string(),
                    tags: vec![],
                    status: None,
                    body: None,
                    key: Some("existing-key".to_string()),
                    dependencies: vec![],
                    summary: None,
                    author: None,
                    assignee: None,
                    executor: None,
                }]),
            })
            .await;
        assert!(result.is_err());
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
                    key: None,
                    dependencies: vec![],
                })
                .await
                .unwrap();
        }

        let result = server
            .handle_list_tasks(ListTasksParams {
                filter: "all".to_string(),
                tag: None,
                plan: Some("plan-a".to_string()),
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
                key: None,
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
                id: Some("task-deadbeef".to_string()),
                key: None,
                plan: None,
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
                content: "new body".to_string(),
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
                content: "after".to_string(),
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
                    key: None,
                    dependencies: vec![],
                })
                .await
                .unwrap();
        }
        server
            .handle_add_note(AddNoteParams {
                plan: "plan-a".to_string(),
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
    async fn add_task_duplicate_key_error() {
        let dir = temp_test_dir("add-task-duplicate-key-error");
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
                key: Some("my-key".to_string()),
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
                key: Some("my-key".to_string()),
                dependencies: vec![],
            })
            .await
            .unwrap_err();

        assert_eq!(err.message, "key 'my-key' already exists in plan 'plan-a'");
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
                key: None,
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
                key: None,
                dependencies: vec![],
            })
            .await
            .unwrap();

        let result = server
            .handle_list_tasks(ListTasksParams {
                filter: "open".to_string(),
                tag: None,
                plan: Some("plan-a".to_string()),
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
                key: None,
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
}

//! Todo MCP server implementation.
//!
//! Stores todos under per-plan directories using YAML front matter + markdown body.
//! Layout: `<data-dir>/<plan>/plan.md`, `<data-dir>/<plan>/todo-<8-hex-id>.md`, and `<data-dir>/<plan>/note-<8-hex-id>.md`.

use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorData, Implementation, ListToolsResult,
    Meta, PaginatedRequestParams, Role, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::schemars::{generate::SchemaGenerator, JsonSchema, Schema};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

// ── Data types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TodoFrontMatter {
    id: String,
    title: String,
    #[serde(default)]
    tags: Vec<String>,
    plan: String,
    #[serde(default = "default_status")]
    status: String,
    #[serde(default)]
    created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    dependencies: Vec<String>,
}

fn default_status() -> String {
    "open".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TodoRecord {
    #[serde(flatten)]
    front: TodoFrontMatter,
    #[serde(default)]
    body: String,
}

#[derive(Debug, Clone, Serialize)]
struct TodoWithBody<'a> {
    id: &'a str,
    title: &'a str,
    tags: &'a [String],
    plan: &'a str,
    status: &'a str,
    created_at: &'a str,
    updated_at: &'a Option<String>,
    key: &'a Option<String>,
    dependencies: &'a [String],
    body: &'a str,
}

// ── Tool parameter structs ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TodoListParams {
    #[serde(default = "default_filter")]
    filter: String,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    plan: Option<String>,
}

fn default_filter() -> String {
    "open".to_string()
}

#[derive(Debug, Deserialize)]
struct TodoGetParams {
    id: String,
}

#[derive(Debug, Deserialize)]
struct TodoCreateParams {
    title: String,
    #[serde(default)]
    tags: Vec<String>,
    plan: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TodoUpdateParams {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    dependencies: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct TodoAppendParams {
    id: String,
    text: String,
}

#[derive(Debug, Deserialize)]
struct TodoDeleteParams {
    id: String,
}

#[derive(Debug, Deserialize)]
struct PlanReadParams {
    name: String,
}

#[derive(Debug, Deserialize)]
struct TodoSpec {
    title: String,
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

#[derive(Debug, Deserialize)]
struct PlanWriteParams {
    name: String,
    content: String,
    #[serde(default)]
    todos: Option<Vec<TodoSpec>>,
}

#[derive(Debug, Deserialize)]
struct PlanAddNoteParams {
    name: String,
    text: String,
}

#[derive(Debug, Deserialize)]
struct PlanGetTodoParams {
    plan: String,
    key: String,
}

#[derive(Debug, Deserialize)]
struct PlanReadNoteParams {
    plan: String,
    note_id: String,
}

macro_rules! impl_json_schema {
    ($ty:ty, $title:expr, $properties_fn:expr, $required:expr) => {
        impl JsonSchema for $ty {
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
    TodoListParams,
    "TodoListParams",
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
    TodoGetParams,
    "TodoGetParams",
    |gen: &mut SchemaGenerator| vec![("id", "Todo ID", gen.subschema_for::<String>()),],
    &["id"]
);

impl_json_schema!(
    TodoCreateParams,
    "TodoCreateParams",
    |gen: &mut SchemaGenerator| vec![
        ("title", "Short title", gen.subschema_for::<String>()),
        ("tags", "Optional tags", gen.subschema_for::<Vec<String>>()),
        (
            "plan",
            "Plan name this todo belongs to (required)",
            gen.subschema_for::<String>()
        ),
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
            "Key that uniquely identifies this todo within its plan",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "dependencies",
            "List of keys of todos this todo depends on (within same plan)",
            gen.subschema_for::<Vec<String>>()
        ),
    ],
    &["title", "plan"]
);

impl_json_schema!(
    TodoUpdateParams,
    "TodoUpdateParams",
    |gen: &mut SchemaGenerator| vec![
        ("id", "Todo ID", gen.subschema_for::<String>()),
        (
            "title",
            "New title (optional)",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "tags",
            "New tags (replaces all, optional)",
            gen.subschema_for::<Option<Vec<String>>>()
        ),
        (
            "plan",
            "New plan name (optional)",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "status",
            "New status (optional)",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "body",
            "New body (replaces full body, optional)",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "key",
            "New key (optional)",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "dependencies",
            "New dependencies (replaces all, optional)",
            gen.subschema_for::<Option<Vec<String>>>()
        ),
    ],
    &["id"]
);

impl_json_schema!(
    TodoAppendParams,
    "TodoAppendParams",
    |gen: &mut SchemaGenerator| vec![
        ("id", "Todo ID", gen.subschema_for::<String>()),
        (
            "text",
            "Text to append to body (markdown)",
            gen.subschema_for::<String>()
        ),
    ],
    &["id", "text"]
);

impl_json_schema!(
    TodoDeleteParams,
    "TodoDeleteParams",
    |gen: &mut SchemaGenerator| vec![("id", "Todo ID", gen.subschema_for::<String>()),],
    &["id"]
);

impl_json_schema!(
    PlanReadParams,
    "PlanReadParams",
    |gen: &mut SchemaGenerator| vec![("name", "Plan name or ID", gen.subschema_for::<String>()),],
    &["name"]
);

impl_json_schema!(
    TodoSpec,
    "TodoSpec",
    |gen: &mut SchemaGenerator| vec![
        ("title", "Short title", gen.subschema_for::<String>()),
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
            "Key that uniquely identifies this todo within its plan",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "dependencies",
            "List of keys of todos this todo depends on (within same plan)",
            gen.subschema_for::<Vec<String>>()
        ),
    ],
    &["title"]
);

impl_json_schema!(
    PlanWriteParams,
    "PlanWriteParams",
    |gen: &mut SchemaGenerator| vec![
        ("name", "Plan name or ID", gen.subschema_for::<String>()),
        (
            "content",
            "Full plan markdown text",
            gen.subschema_for::<String>()
        ),
        (
            "todos",
            "Optional list of todos to create along with plan",
            gen.subschema_for::<Option<Vec<TodoSpec>>>()
        ),
    ],
    &["name", "content"]
);

impl_json_schema!(
    PlanAddNoteParams,
    "PlanAddNoteParams",
    |gen: &mut SchemaGenerator| vec![
        ("name", "Plan name or ID", gen.subschema_for::<String>()),
        (
            "text",
            "Text to append as a note",
            gen.subschema_for::<String>()
        ),
    ],
    &["name", "text"]
);

impl_json_schema!(
    PlanGetTodoParams,
    "PlanGetTodoParams",
    |gen: &mut SchemaGenerator| vec![
        ("plan", "Plan name or ID", gen.subschema_for::<String>()),
        ("key", "Todo key within plan", gen.subschema_for::<String>()),
    ],
    &["plan", "key"]
);

impl_json_schema!(
    PlanReadNoteParams,
    "PlanReadNoteParams",
    |gen: &mut SchemaGenerator| vec![
        ("plan", "Plan name or ID", gen.subschema_for::<String>()),
        ("note_id", "Note ID (8-hex string or note-<id> prefix)", gen.subschema_for::<String>()),
    ],
    &["plan", "note_id"]
);

fn plan_dir(dir: &Path, plan_name: &str) -> PathBuf {
    dir.join(plan_name)
}

fn todo_file_path(dir: &Path, plan_name: &str, id: &str) -> PathBuf {
    plan_dir(dir, plan_name).join(format!("todo-{}.md", normalize_id(id)))
}

fn plan_file_path(dir: &Path, plan_name: &str) -> PathBuf {
    plan_dir(dir, plan_name).join("plan.md")
}

fn note_file_path(dir: &Path, plan_name: &str, id: &str) -> PathBuf {
    plan_dir(dir, plan_name).join(format!("note-{}.md", id))
}

fn parse_yaml_frontmatter(content: &str) -> Result<(TodoFrontMatter, String), String> {
    let normalized;
    let content = if content.contains('\r') {
        normalized = content.replace("\r\n", "\n").replace('\r', "\n");
        normalized.as_str()
    } else {
        content
    };

    if !content.starts_with("---\n") {
        return Err("missing YAML front matter".to_string());
    }

    let rest = &content[4..];
    let end = rest
        .find("\n---\n")
        .ok_or_else(|| "missing YAML front matter closing delimiter".to_string())?;
    let yaml = &rest[..end];
    let body = rest[end + 5..].to_string();
    let front = serde_yaml::from_str::<TodoFrontMatter>(yaml)
        .map_err(|err| format!("invalid YAML front matter: {err}"))?;
    Ok((front, body))
}

fn serialize_todo(todo: &TodoRecord) -> Result<String, String> {
    let yaml = serde_yaml::to_string(&todo.front)
        .map_err(|err| format!("failed to serialize YAML front matter: {err}"))?;
    Ok(format!(
        "---
{}---
{}",
        yaml, todo.body
    ))
}

fn read_todo(dir: &Path, plan_name: &str, id: &str) -> Result<TodoRecord, String> {
    let id = normalize_id(id);
    let path = todo_file_path(dir, plan_name, &id);
    let content = std::fs::read_to_string(&path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let (mut front, body) = parse_yaml_frontmatter(&content)?;
    front.id = normalize_id(&front.id);
    front.plan = normalize_plan_name(&front.plan)?;
    Ok(TodoRecord { front, body })
}

fn write_todo(dir: &Path, todo: &TodoRecord) -> Result<(), String> {
    let plan = normalize_plan_name(&todo.front.plan)?;
    let plan_path = plan_dir(dir, &plan);
    std::fs::create_dir_all(&plan_path)
        .map_err(|err| format!("failed to create {}: {err}", plan_path.display()))?;

    let mut normalized = todo.clone();
    normalized.front.id = normalize_id(&normalized.front.id);
    normalized.front.plan = plan;
    let path = todo_file_path(dir, &normalized.front.plan, &normalized.front.id);
    let content = serialize_todo(&normalized)?;
    std::fs::write(&path, content)
        .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn plan_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort();
    dirs
}

fn find_todo_file(dir: &Path, id: &str) -> Result<(String, PathBuf), String> {
    let id = normalize_id(id);
    let file_name = format!("todo-{id}.md");
    for plan_path in plan_dirs(dir) {
        let candidate = plan_path.join(&file_name);
        if candidate.is_file() {
            let plan_name = plan_path
                .file_name()
                .and_then(OsStr::to_str)
                .ok_or_else(|| format!("invalid plan directory name: {}", plan_path.display()))?
                .to_string();
            return Ok((plan_name, candidate));
        }
    }
    Err(format!("Todo {} not found", display_id(&id)))
}

fn read_todo_by_id(dir: &Path, id: &str) -> Result<TodoRecord, String> {
    let (plan_name, _) = find_todo_file(dir, id)?;
    read_todo(dir, &plan_name, id)
}

fn list_todos(
    dir: &Path,
    plan_filter: Option<&str>,
    tag_filter: Option<&str>,
    status_filter: Option<&str>,
) -> Vec<TodoRecord> {
    let normalized_plan = plan_filter.and_then(|plan| normalize_plan_name(plan).ok());
    let normalized_tag = tag_filter.map(|tag| tag.to_ascii_lowercase());
    let normalized_status = status_filter.map(|status| status.to_ascii_lowercase());

    let mut todos = Vec::new();
    let plans = if let Some(plan) = normalized_plan.as_deref() {
        vec![plan_dir(dir, plan)]
    } else {
        plan_dirs(dir)
    };

    for plan_path in plans {
        let Ok(entries) = std::fs::read_dir(&plan_path) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            if !path.is_file() || !name.starts_with("todo-") || !name.ends_with(".md") {
                continue;
            }
            let Some(plan_name) = plan_path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            let Ok(todo) = read_todo(dir, plan_name, &name[5..name.len() - 3]) else {
                continue;
            };

            let matches_tag = normalized_tag.as_ref().is_none_or(|tag| {
                todo.front
                    .tags
                    .iter()
                    .any(|existing| existing.eq_ignore_ascii_case(tag))
            });
            let matches_status = normalized_status
                .as_ref()
                .is_none_or(|status| todo.front.status.eq_ignore_ascii_case(status));

            if matches_tag && matches_status {
                todos.push(todo);
            }
        }
    }

    todos.sort_by(|a, b| {
        a.front
            .created_at
            .cmp(&b.front.created_at)
            .then(a.front.id.cmp(&b.front.id))
    });
    todos
}

fn is_closed(status: &str) -> bool {
    matches!(status.to_ascii_lowercase().as_str(), "closed" | "done")
}

fn find_todo_by_key(dir: &Path, plan_name: &str, key: &str) -> Option<TodoRecord> {
    list_todos(dir, Some(plan_name), None, None)
        .into_iter()
        .find(|todo| todo.front.key.as_deref() == Some(key))
}

fn normalize_plan_name(plan: &str) -> Result<String, String> {
    let plan = plan.trim();
    if plan.is_empty() {
        return Err("Plan name cannot be empty".to_string());
    }
    if plan.contains('/') || plan.contains('\\') || plan.contains("..") {
        return Err("Invalid plan name".to_string());
    }
    Ok(plan.to_string())
}

fn todo_to_json(todo: &TodoRecord) -> Value {
    serde_json::to_value(TodoWithBody {
        id: &todo.front.id,
        title: &todo.front.title,
        tags: &todo.front.tags,
        plan: &todo.front.plan,
        status: &todo.front.status,
        created_at: &todo.front.created_at,
        updated_at: &todo.front.updated_at,
        key: &todo.front.key,
        dependencies: &todo.front.dependencies,
        body: &todo.body,
    })
    .unwrap_or_else(|_| json!({}))
}

fn todo_list_to_json(todos: &[TodoRecord]) -> Value {
    Value::Array(todos.iter().map(todo_to_json).collect())
}

fn result_text(text: String, summary: String) -> CallToolResult {
    CallToolResult::success(vec![
        Content::text(text).with_audience(vec![Role::Assistant]),
        Content::text(summary).with_audience(vec![Role::User]),
    ])
}

fn result_json(value: Value, summary: String) -> CallToolResult {
    result_text(
        serde_json::to_string_pretty(&value).unwrap_or_default(),
        summary,
    )
}

pub struct TodoServer {
    dir: PathBuf,
}

impl TodoServer {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn handle_list(&self, params: TodoListParams) -> Result<CallToolResult, ErrorData> {
        let status_filter = match params.filter.as_str() {
            "all" | "open" | "closed" | "done" => None,
            other => Some(other),
        };

        let mut filtered = list_todos(
            &self.dir,
            params.plan.as_deref(),
            params.tag.as_deref(),
            status_filter,
        );
        if params.filter == "open" {
            filtered.retain(|todo| !is_closed(&todo.front.status));
        } else if matches!(params.filter.as_str(), "closed" | "done") {
            filtered.retain(|todo| is_closed(&todo.front.status));
        }

        let count = filtered.len();
        Ok(result_json(
            todo_list_to_json(&filtered),
            format!("Found {count} todos"),
        ))
    }

    fn handle_get(&self, params: TodoGetParams) -> Result<CallToolResult, ErrorData> {
        match read_todo_by_id(&self.dir, &params.id) {
            Ok(todo) => Ok(result_json(
                todo_to_json(&todo),
                format!(
                    "{}: {} [{}]",
                    display_id(&todo.front.id),
                    todo.front.title,
                    todo.front.status
                ),
            )),
            Err(err) => Ok(CallToolResult::error(vec![Content::text(err)])),
        }
    }

    fn handle_create(&self, params: TodoCreateParams) -> Result<CallToolResult, ErrorData> {
        let plan = normalize_plan_name(&params.plan)
            .map_err(|err| ErrorData::invalid_params(err, None))?;
        let id = generate_id();
        let todo = TodoRecord {
            front: TodoFrontMatter {
                id: id.clone(),
                title: params.title,
                tags: params.tags,
                plan,
                status: params.status.unwrap_or_else(default_status),
                created_at: now_iso(),
                updated_at: None,
                key: params.key,
                dependencies: params.dependencies,
            },
            body: params.body.unwrap_or_default(),
        };
        write_todo(&self.dir, &todo).map_err(|err| ErrorData::internal_error(err, None))?;
        Ok(result_json(
            todo_to_json(&todo),
            format!("Created {}", display_id(&todo.front.id)),
        ))
    }

    fn handle_update(&self, params: TodoUpdateParams) -> Result<CallToolResult, ErrorData> {
        let old_todo = read_todo_by_id(&self.dir, &params.id)
            .map_err(|err| ErrorData::invalid_params(err, None))?;
        let old_plan = old_todo.front.plan.clone();
        let old_id = old_todo.front.id.clone();
        let mut todo = old_todo.clone();

        if let Some(title) = params.title {
            todo.front.title = title;
        }
        if let Some(tags) = params.tags {
            todo.front.tags = tags;
        }
        if let Some(plan) = params.plan {
            todo.front.plan =
                normalize_plan_name(&plan).map_err(|err| ErrorData::invalid_params(err, None))?;
        }
        if let Some(status) = params.status {
            todo.front.status = status;
        }
        if let Some(body) = params.body {
            todo.body = body;
        }
        if let Some(key) = params.key {
            todo.front.key = Some(key);
        }
        if let Some(dependencies) = params.dependencies {
            todo.front.dependencies = dependencies;
        }
        todo.front.updated_at = Some(now_iso());

        write_todo(&self.dir, &todo).map_err(|err| ErrorData::internal_error(err, None))?;
        if old_plan != todo.front.plan {
            let old_path = todo_file_path(&self.dir, &old_plan, &old_id);
            if old_path.exists() {
                std::fs::remove_file(&old_path).map_err(|err| {
                    ErrorData::internal_error(
                        format!("failed to remove {}: {err}", old_path.display()),
                        None,
                    )
                })?;
            }
        }

        Ok(result_json(
            todo_to_json(&todo),
            format!("Updated {}", display_id(&todo.front.id)),
        ))
    }

    fn handle_append(&self, params: TodoAppendParams) -> Result<CallToolResult, ErrorData> {
        let mut todo = read_todo_by_id(&self.dir, &params.id)
            .map_err(|err| ErrorData::invalid_params(err, None))?;
        if todo.body.is_empty() {
            todo.body = params.text;
        } else {
            todo.body.push_str(&params.text);
        }
        todo.front.updated_at = Some(now_iso());
        write_todo(&self.dir, &todo).map_err(|err| ErrorData::internal_error(err, None))?;
        Ok(result_json(
            todo_to_json(&todo),
            format!("Appended to {}", display_id(&todo.front.id)),
        ))
    }

    fn handle_delete(&self, params: TodoDeleteParams) -> Result<CallToolResult, ErrorData> {
        let (_, path) = find_todo_file(&self.dir, &params.id)
            .map_err(|err| ErrorData::invalid_params(err, None))?;
        std::fs::remove_file(&path).map_err(|err| {
            ErrorData::internal_error(format!("failed to delete {}: {err}", path.display()), None)
        })?;
        Ok(result_text(
            format!("Deleted {}", display_id(&params.id)),
            format!("Deleted {}", display_id(&params.id)),
        ))
    }

    fn handle_plan_read(&self, params: PlanReadParams) -> Result<CallToolResult, ErrorData> {
        let name = normalize_plan_name(&params.name)
            .map_err(|err| ErrorData::invalid_params(err, None))?;
        let path = plan_file_path(&self.dir, &name);
        let dir = plan_dir(&self.dir, &name);
        if !dir.exists() {
            return Err(ErrorData::invalid_params(
                format!("plan '{name}' not found"),
                None,
            ));
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => {
                return Err(ErrorData::internal_error(
                    format!("failed to read {}: {err}", path.display()),
                    None,
                ))
            }
        };
        let mut note_ids = Vec::new();
        let mut todo_ids = Vec::new();

        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let file_name = entry.file_name().to_string_lossy().into_owned();
                if let Some(note_id) = file_name
                    .strip_prefix("note-")
                    .and_then(|name| name.strip_suffix(".md"))
                {
                    note_ids.push(note_id.to_string());
                } else if let Some(todo_id) = file_name
                    .strip_prefix("todo-")
                    .and_then(|name| name.strip_suffix(".md"))
                {
                    todo_ids.push(todo_id.to_string());
                }
            }
        }

        note_ids.sort();
        todo_ids.sort();

        Ok(result_json(
            json!({
                "plan": name,
                "content": content,
                "note_ids": note_ids,
                "todo_ids": todo_ids,
            }),
            format!("Read plan {name}"),
        ))
    }

    fn handle_plan_write(&self, params: PlanWriteParams) -> Result<CallToolResult, ErrorData> {
        let name = normalize_plan_name(&params.name)
            .map_err(|err| ErrorData::invalid_params(err, None))?;
        let dir = plan_dir(&self.dir, &name);
        std::fs::create_dir_all(&dir).map_err(|err| {
            ErrorData::internal_error(format!("failed to create {}: {err}", dir.display()), None)
        })?;
        let path = plan_file_path(&self.dir, &name);
        std::fs::write(&path, params.content).map_err(|err| {
            ErrorData::internal_error(format!("failed to write {}: {err}", path.display()), None)
        })?;

        let mut created = 0usize;
        for spec in params.todos.unwrap_or_default() {
            let TodoSpec {
                title,
                tags,
                status,
                body,
                key,
                dependencies,
            } = spec;

            let todo = if let Some(key) = key {
                if let Some(mut existing) = find_todo_by_key(&self.dir, &name, &key) {
                    existing.front.title = title;
                    if !tags.is_empty() {
                        existing.front.tags = tags;
                    }
                    existing.front.plan = name.clone();
                    if let Some(status) = status {
                        existing.front.status = status;
                    }
                    existing.front.updated_at = Some(now_iso());
                    existing.front.key = Some(key);
                    if !dependencies.is_empty() {
                        existing.front.dependencies = dependencies;
                    }
                    if let Some(body) = body {
                        existing.body = body;
                    }
                    existing
                } else {
                    created += 1;
                    TodoRecord {
                        front: TodoFrontMatter {
                            id: generate_id(),
                            title,
                            tags,
                            plan: name.clone(),
                            status: status.unwrap_or_else(default_status),
                            created_at: now_iso(),
                            updated_at: None,
                            key: Some(key),
                            dependencies,
                        },
                        body: body.unwrap_or_default(),
                    }
                }
            } else {
                created += 1;
                TodoRecord {
                    front: TodoFrontMatter {
                        id: generate_id(),
                        title,
                        tags,
                        plan: name.clone(),
                        status: status.unwrap_or_else(default_status),
                        created_at: now_iso(),
                        updated_at: None,
                        key: None,
                        dependencies,
                    },
                    body: body.unwrap_or_default(),
                }
            };

            write_todo(&self.dir, &todo).map_err(|err| ErrorData::internal_error(err, None))?;
        }

        Ok(result_text(
            format!("Wrote plan {name}"),
            format!("Wrote plan {name} and created {created} todos"),
        ))
    }

    fn handle_plan_add_note(&self, params: PlanAddNoteParams) -> Result<CallToolResult, ErrorData> {
        let name = normalize_plan_name(&params.name)
            .map_err(|err| ErrorData::invalid_params(err, None))?;
        let id = generate_id();
        let dir = plan_dir(&self.dir, &name);
        std::fs::create_dir_all(&dir).map_err(|err| {
            ErrorData::internal_error(format!("failed to create {}: {err}", dir.display()), None)
        })?;
        let path = note_file_path(&self.dir, &name, &id);
        std::fs::write(&path, &params.text).map_err(|err| {
            ErrorData::internal_error(format!("failed to write {}: {err}", path.display()), None)
        })?;
        Ok(result_text(
            format!("Created note {id} in plan {name}"),
            format!("Created note {id} in plan {name}"),
        ))
    }

    fn handle_plan_get_todo(&self, params: PlanGetTodoParams) -> Result<CallToolResult, ErrorData> {
        let plan = normalize_plan_name(&params.plan)
            .map_err(|err| ErrorData::invalid_params(err, None))?;
        let matching: Vec<TodoRecord> = list_todos(&self.dir, Some(&plan), None, None)
            .into_iter()
            .filter(|todo| todo.front.key.as_deref() == Some(params.key.as_str()))
            .collect();

        if matching.is_empty() {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "No todo with key '{}' found in plan '{}'",
                params.key, plan
            ))]));
        }
        if matching.len() > 1 {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Multiple todos with key '{}' found in plan '{}' - this indicates data inconsistency",
                params.key, plan
            ))]));
        }

        let todo = &matching[0];
        Ok(result_json(
            todo_to_json(todo),
            format!("Found {}: {}", display_id(&todo.front.id), todo.front.title),
        ))
    }

    fn handle_plan_read_note(&self, params: PlanReadNoteParams) -> Result<CallToolResult, ErrorData> {
        let plan = normalize_plan_name(&params.plan)
            .map_err(|err| ErrorData::invalid_params(err, None))?;
        let raw_id = params.note_id.trim();
        let id = raw_id
            .strip_prefix("note-")
            .unwrap_or(raw_id)
            .to_ascii_lowercase();
        let path = note_file_path(&self.dir, &plan, &id);
        let content = std::fs::read_to_string(&path).map_err(|_| {
            ErrorData::invalid_params(format!("note '{id}' not found in plan '{plan}'"), None)
        })?;
        Ok(result_text(content, format!("Read note {id} in plan {plan}")))
    }
}

impl ServerHandler for TodoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "harnx-mcp-todo",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "File-based todo/plan management. Todos stored as markdown files with YAML front matter in per-plan directories.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            meta: None,
            next_cursor: None,
            tools: vec![
                Tool::new(
                    "todo_list",
                    "List todos. Filter by status ('open', 'closed', 'all') and optionally by tag or plan.",
                    Map::new(),
                )
                .with_input_schema::<TodoListParams>()
                .with_meta(Meta(json!({
                    "call_template": "todos{% if args.filter %} [{{ args.filter }}]{% endif %}{% if args.plan %} plan={{ args.plan }}{% endif %}",
                    "result_template": "{{ result.content[0].text | default('') }}"
                }).as_object().unwrap().clone())),
                Tool::new(
                    "todo_get",
                    "Get single todo by ID, including full body.",
                    Map::new(),
                )
                .with_input_schema::<TodoGetParams>()
                .with_meta(Meta(json!({
                    "call_template": "todo {{ args.id }}",
                    "result_template": "{{ result.content[0].text | default('') }}"
                }).as_object().unwrap().clone())),
                Tool::new(
                    "todo_create",
                    "Create new todo with title, required plan, optional tags, status, body, key, and dependencies.",
                    Map::new(),
                )
                .with_input_schema::<TodoCreateParams>()
                .with_meta(Meta(json!({
                    "call_template": "+ todo {{ args.title }}{% if args.plan %} ({{ args.plan }}){% endif %}",
                    "result_template": "{{ result.content[0].text | default('') }}"
                }).as_object().unwrap().clone())),
                Tool::new(
                    "todo_update",
                    "Update todo title, status, tags, plan, key, dependencies, or body (replaces).",
                    Map::new(),
                )
                .with_input_schema::<TodoUpdateParams>()
                .with_meta(Meta(json!({
                    "call_template": "* todo {{ args.id }}{% if args.title %} \"{{ args.title }}\"{% endif %}",
                    "result_template": "{{ result.content[0].text | default('') }}"
                }).as_object().unwrap().clone())),
                Tool::new(
                    "todo_append",
                    "Append text to todo body (adds, doesn't replace).",
                    Map::new(),
                )
                .with_input_schema::<TodoAppendParams>()
                .with_meta(Meta(json!({
                    "call_template": ">> todo {{ args.id }}",
                    "result_template": "{{ result.content[0].text | default('') }}"
                }).as_object().unwrap().clone())),
                Tool::new("todo_delete", "Delete todo by ID.", Map::new())
                    .with_input_schema::<TodoDeleteParams>()
                    .with_meta(Meta(json!({
                        "call_template": "- todo {{ args.id }}",
                        "result_template": "{{ result.content[0].text | default('') }}"
                    }).as_object().unwrap().clone())),
                Tool::new("read_plan", "Read plan markdown file and list available note and todo IDs.", Map::new())
                    .with_input_schema::<PlanReadParams>()
                    .with_meta(Meta(json!({
                        "call_template": "plan {{ args.name }}",
                        "result_template": "{{ result.content[0].text | default('') }}"
                    }).as_object().unwrap().clone())),
                Tool::new(
                    "write_plan",
                    "Write full plan markdown text and optionally create batch todos in that plan.",
                    Map::new(),
                )
                .with_input_schema::<PlanWriteParams>()
                .with_meta(Meta(json!({
                    "call_template": "+ plan {{ args.name }}",
                    "result_template": "{{ result.content[0].text | default('') }}"
                }).as_object().unwrap().clone())),
                Tool::new(
                    "plan_add_note",
                    "Append note section to plan markdown as a dedicated note file.",
                    Map::new(),
                )
                .with_input_schema::<PlanAddNoteParams>()
                .with_meta(Meta(json!({
                    "call_template": ">> plan {{ args.name }}",
                    "result_template": "{{ result.content[0].text | default('') }}"
                }).as_object().unwrap().clone())),
                Tool::new(
                    "plan_get_todo",
                    "Get todo in plan by key field.",
                    Map::new(),
                )
                .with_input_schema::<PlanGetTodoParams>()
                .with_meta(Meta(json!({
                    "call_template": "plan {{ args.plan }} / {{ args.key }}",
                    "result_template": "{{ result.content[0].text | default('') }}"
                }).as_object().unwrap().clone())),
                Tool::new(
                    "plan_read_note",
                    "Read a note file in a plan by note ID.",
                    Map::new(),
                )
                .with_input_schema::<PlanReadNoteParams>()
                .with_meta(Meta(json!({
                    "call_template": "note {{ args.plan }}/{{ args.note_id }}",
                    "result_template": "{{ result.content[0].text | default('') }}"
                }).as_object().unwrap().clone())),
            ],
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        match request.name.as_ref() {
            "todo_list" => {
                let params = parse_arguments::<TodoListParams>(request.arguments)?;
                self.handle_list(params)
            }
            "todo_get" => {
                let params = parse_arguments::<TodoGetParams>(request.arguments)?;
                self.handle_get(params)
            }
            "todo_create" => {
                let params = parse_arguments::<TodoCreateParams>(request.arguments)?;
                self.handle_create(params)
            }
            "todo_update" => {
                let params = parse_arguments::<TodoUpdateParams>(request.arguments)?;
                self.handle_update(params)
            }
            "todo_append" => {
                let params = parse_arguments::<TodoAppendParams>(request.arguments)?;
                self.handle_append(params)
            }
            "todo_delete" => {
                let params = parse_arguments::<TodoDeleteParams>(request.arguments)?;
                self.handle_delete(params)
            }
            "read_plan" => {
                let params = parse_arguments::<PlanReadParams>(request.arguments)?;
                self.handle_plan_read(params)
            }
            "write_plan" => {
                let params = parse_arguments::<PlanWriteParams>(request.arguments)?;
                self.handle_plan_write(params)
            }
            "plan_add_note" => {
                let params = parse_arguments::<PlanAddNoteParams>(request.arguments)?;
                self.handle_plan_add_note(params)
            }
            "plan_get_todo" => {
                let params = parse_arguments::<PlanGetTodoParams>(request.arguments)?;
                self.handle_plan_get_todo(params)
            }
            "plan_read_note" => {
                let params = parse_arguments::<PlanReadNoteParams>(request.arguments)?;
                self.handle_plan_read_note(params)
            }
            other => Err(ErrorData::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }

    async fn on_roots_list_changed(&self, _context: NotificationContext<RoleServer>) {}
}

// ── Utility functions ───────────────────────────────────────────────────────

fn parse_arguments<T: serde::de::DeserializeOwned>(
    arguments: Option<Map<String, Value>>,
) -> Result<T, ErrorData> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|err| ErrorData::invalid_params(format!("invalid arguments: {err}"), None))
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

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn generate_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    format!("{:08x}", hasher.finish() as u32)
}

fn normalize_id(id: &str) -> String {
    let trimmed = id.trim();
    let without_prefix = trimmed
        .strip_prefix("todo-")
        .or_else(|| trimmed.strip_prefix("TODO-"))
        .unwrap_or(trimmed);
    without_prefix.to_ascii_lowercase()
}

fn display_id(id: &str) -> String {
    format!("TODO-{}", normalize_id(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_test_dir(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let unique = format!(
            "harnx-mcp-todo-{name}-{}-{}-{}",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed),
            unique_iso()
        );
        path.push(unique);
        if path.exists() {
            fs::remove_dir_all(&path).unwrap();
        }
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn unique_iso() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{nanos}")
    }

    fn sample_todo(plan: &str, id: &str, status: &str, key: Option<&str>) -> TodoRecord {
        TodoRecord {
            front: TodoFrontMatter {
                id: id.to_string(),
                title: format!("todo-{id}"),
                tags: vec!["tag1".to_string()],
                plan: plan.to_string(),
                status: status.to_string(),
                created_at: unique_iso(),
                updated_at: Some(unique_iso()),
                key: key.map(str::to_string),
                dependencies: vec!["dep-1".to_string()],
            },
            body: format!("body-{id}"),
        }
    }

    fn extract_text(result: CallToolResult) -> String {
        result.content[0]
            .raw
            .as_text()
            .map(|text| text.text.clone())
            .unwrap_or_else(|| panic!("unexpected content: {:?}", result.content[0]))
    }

    #[test]
    fn serialize_and_parse_yaml_frontmatter_roundtrip_and_crlf() {
        let todo = sample_todo("alpha", "deadbeef", "open", Some("task-1"));
        let serialized = serialize_todo(&todo).unwrap();
        let (front, body) = parse_yaml_frontmatter(&serialized).unwrap();
        assert_eq!(front.id, todo.front.id);
        assert_eq!(front.title, todo.front.title);
        assert_eq!(front.tags, todo.front.tags);
        assert_eq!(front.plan, todo.front.plan);
        assert_eq!(front.status, todo.front.status);
        assert_eq!(front.created_at, todo.front.created_at);
        assert_eq!(front.updated_at, todo.front.updated_at);
        assert_eq!(front.key, todo.front.key);
        assert_eq!(front.dependencies, todo.front.dependencies);
        assert_eq!(body, todo.body);

        let crlf = serialized.replace('\n', "\r\n");
        let (crlf_front, crlf_body) = parse_yaml_frontmatter(&crlf).unwrap();
        assert_eq!(crlf_front.id, todo.front.id);
        assert_eq!(crlf_front.key, todo.front.key);
        assert_eq!(crlf_body, todo.body);
    }

    #[test]
    fn list_todos_status_filter_matches_literal_statuses() {
        let dir = temp_test_dir("list-status-filter");
        write_todo(&dir, &sample_todo("plan-a", "00000001", "open", None)).unwrap();
        write_todo(&dir, &sample_todo("plan-a", "00000002", "closed", None)).unwrap();
        write_todo(&dir, &sample_todo("plan-a", "00000003", "done", None)).unwrap();

        let open = list_todos(&dir, Some("plan-a"), None, Some("open"));
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].front.status, "open");
        assert!(open.iter().all(|todo| !is_closed(&todo.front.status)));

        let closed = list_todos(&dir, Some("plan-a"), None, Some("closed"));
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].front.status, "closed");

        let done = list_todos(&dir, Some("plan-a"), None, Some("done"));
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].front.status, "done");
    }

    #[test]
    fn handle_list_returns_created_todo() {
        let dir = temp_test_dir("handle-list");
        let server = TodoServer::new(dir);

        server
            .handle_create(TodoCreateParams {
                title: "first todo".to_string(),
                tags: vec!["ship".to_string()],
                plan: "plan-a".to_string(),
                status: None,
                body: Some("details".to_string()),
                key: Some("task-1".to_string()),
                dependencies: vec![],
            })
            .unwrap();

        let result = server
            .handle_list(TodoListParams {
                filter: "all".to_string(),
                tag: None,
                plan: Some("plan-a".to_string()),
            })
            .unwrap();
        let text = extract_text(result);
        let todos: Vec<Value> = serde_json::from_str(&text).unwrap();
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0]["title"], "first todo");
        assert_eq!(todos[0]["plan"], "plan-a");
    }

    #[test]
    fn write_plan_upserts_todo_by_key() {
        let dir = temp_test_dir("write-plan-upsert");
        let server = TodoServer::new(dir.clone());

        server
            .handle_plan_write(PlanWriteParams {
                name: "plan-a".to_string(),
                content: "# plan\n".to_string(),
                todos: Some(vec![TodoSpec {
                    title: "first".to_string(),
                    tags: vec!["alpha".to_string()],
                    status: Some("open".to_string()),
                    body: Some("body one".to_string()),
                    key: Some("task-1".to_string()),
                    dependencies: vec!["dep-a".to_string()],
                }]),
            })
            .unwrap();

        server
            .handle_plan_write(PlanWriteParams {
                name: "plan-a".to_string(),
                content: "# plan updated\n".to_string(),
                todos: Some(vec![TodoSpec {
                    title: "second".to_string(),
                    tags: vec!["beta".to_string()],
                    status: Some("done".to_string()),
                    body: Some("body two".to_string()),
                    key: Some("task-1".to_string()),
                    dependencies: vec!["dep-b".to_string()],
                }]),
            })
            .unwrap();

        let todo_files: Vec<_> = fs::read_dir(dir.join("plan-a"))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("todo-")
            })
            .collect();
        assert_eq!(todo_files.len(), 1);

        let todo = find_todo_by_key(&dir, "plan-a", "task-1").unwrap();
        assert_eq!(todo.front.title, "second");
        assert_eq!(todo.front.status, "done");
        assert_eq!(todo.front.tags, vec!["beta".to_string()]);
        assert_eq!(todo.front.dependencies, vec!["dep-b".to_string()]);
        assert_eq!(todo.body, "body two");
    }

    #[test]
    fn write_plan_upsert_preserves_omitted_optional_fields() {
        let dir = temp_test_dir("write-plan-upsert-preserve");
        let server = TodoServer::new(dir.clone());

        server
            .handle_plan_write(PlanWriteParams {
                name: "plan-a".to_string(),
                content: "# plan\n".to_string(),
                todos: Some(vec![TodoSpec {
                    title: "first".to_string(),
                    tags: vec!["alpha".to_string()],
                    status: Some("done".to_string()),
                    body: Some("body one".to_string()),
                    key: Some("task-1".to_string()),
                    dependencies: vec!["dep-a".to_string()],
                }]),
            })
            .unwrap();

        server
            .handle_plan_write(PlanWriteParams {
                name: "plan-a".to_string(),
                content: "# plan updated\n".to_string(),
                todos: Some(vec![TodoSpec {
                    title: "second".to_string(),
                    tags: vec![],
                    status: None,
                    body: None,
                    key: Some("task-1".to_string()),
                    dependencies: vec![],
                }]),
            })
            .unwrap();

        let todo = find_todo_by_key(&dir, "plan-a", "task-1").unwrap();
        assert_eq!(todo.front.title, "second");
        assert_eq!(todo.front.status, "done");
        assert_eq!(todo.front.tags, vec!["alpha".to_string()]);
        assert_eq!(todo.front.dependencies, vec!["dep-a".to_string()]);
        assert_eq!(todo.body, "body one");
    }

    #[test]
    fn todo_get_finds_todo_across_plan_directories() {
        let dir = temp_test_dir("todo-get-cross-plan");
        let server = TodoServer::new(dir);

        let created = server
            .handle_create(TodoCreateParams {
                title: "second todo".to_string(),
                tags: vec![],
                plan: "plan-b".to_string(),
                status: None,
                body: Some("details".to_string()),
                key: None,
                dependencies: vec![],
            })
            .unwrap();
        let created_todo: Value = serde_json::from_str(&extract_text(created)).unwrap();

        let result = server
            .handle_get(TodoGetParams {
                id: created_todo["id"].as_str().unwrap().to_string(),
            })
            .unwrap();
        let fetched_todo: Value = serde_json::from_str(&extract_text(result)).unwrap();
        assert_eq!(fetched_todo["plan"], "plan-b");
    }

    #[test]
    fn todo_update_plan_move_moves_file_to_new_plan_directory() {
        let dir = temp_test_dir("todo-update-plan-move");
        let server = TodoServer::new(dir.clone());

        let created = server
            .handle_create(TodoCreateParams {
                title: "movable todo".to_string(),
                tags: vec![],
                plan: "plan-a".to_string(),
                status: None,
                body: None,
                key: None,
                dependencies: vec![],
            })
            .unwrap();
        let created_todo: Value = serde_json::from_str(&extract_text(created)).unwrap();
        let id = created_todo["id"].as_str().unwrap().to_string();

        server
            .handle_update(TodoUpdateParams {
                id: id.clone(),
                title: None,
                tags: None,
                plan: Some("plan-b".to_string()),
                status: None,
                body: None,
                key: None,
                dependencies: None,
            })
            .unwrap();

        assert!(!dir.join("plan-a").join(format!("todo-{id}.md")).exists());
        assert!(dir.join("plan-b").join(format!("todo-{id}.md")).exists());
    }

    #[test]
    fn todo_delete_removes_file_from_plan_directory() {
        let dir = temp_test_dir("todo-delete");
        let server = TodoServer::new(dir.clone());

        let created = server
            .handle_create(TodoCreateParams {
                title: "delete me".to_string(),
                tags: vec![],
                plan: "plan-a".to_string(),
                status: None,
                body: None,
                key: None,
                dependencies: vec![],
            })
            .unwrap();
        let created_todo: Value = serde_json::from_str(&extract_text(created)).unwrap();
        let id = created_todo["id"].as_str().unwrap().to_string();

        server
            .handle_delete(TodoDeleteParams { id: id.clone() })
            .unwrap();

        assert!(!dir.join("plan-a").join(format!("todo-{id}.md")).exists());
    }

    #[test]
    fn todo_append_appends_text_to_body() {
        let dir = temp_test_dir("todo-append");
        let server = TodoServer::new(dir.clone());

        let created = server
            .handle_create(TodoCreateParams {
                title: "append me".to_string(),
                tags: vec![],
                plan: "plan-a".to_string(),
                status: None,
                body: Some("hello".to_string()),
                key: None,
                dependencies: vec![],
            })
            .unwrap();
        let created_todo: Value = serde_json::from_str(&extract_text(created)).unwrap();
        let id = created_todo["id"].as_str().unwrap().to_string();

        server
            .handle_append(TodoAppendParams {
                id: id.clone(),
                text: " world".to_string(),
            })
            .unwrap();

        let todo = read_todo(&dir, "plan-a", &id).unwrap();
        assert_eq!(todo.body, "hello world");
    }

    #[test]
    fn list_todos_without_plan_filter_returns_multiple_plan_directories() {
        let dir = temp_test_dir("list-cross-plan");
        let server = TodoServer::new(dir);

        for plan in ["plan-a", "plan-b"] {
            server
                .handle_create(TodoCreateParams {
                    title: format!("todo for {plan}"),
                    tags: vec![],
                    plan: plan.to_string(),
                    status: None,
                    body: None,
                    key: None,
                    dependencies: vec![],
                })
                .unwrap();
        }

        let result = server
            .handle_list(TodoListParams {
                filter: "all".to_string(),
                tag: None,
                plan: None,
            })
            .unwrap();
        let todos: Vec<Value> = serde_json::from_str(&extract_text(result)).unwrap();
        assert_eq!(todos.len(), 2);
    }

    #[test]
    fn plan_add_note_creates_missing_plan_file() {
        let dir = temp_test_dir("plan-add-note-not-found");
        let server = TodoServer::new(dir.clone());

        let result = server
            .handle_plan_add_note(PlanAddNoteParams {
                name: "plan-a".to_string(),
                text: "hello note".to_string(),
            })
            .unwrap();
        let summary = extract_text(result);
        assert!(summary.contains("Created note"));
        assert!(summary.contains("plan-a"));

        let note_id = summary
            .split("note ")
            .nth(1)
            .unwrap()
            .split(" in plan")
            .next()
            .unwrap();
        let note_path = dir.join("plan-a").join(format!("note-{note_id}.md"));
        assert!(note_path.exists());

        let content = fs::read_to_string(&note_path).unwrap();
        assert!(content.contains("hello note"));
        assert!(!dir.join("plan-a").join("plan.md").exists());
    }

    #[test]
    fn handle_list_closed_bucket_includes_done_and_closed() {
        let dir = temp_test_dir("handle-list-buckets");
        let server = TodoServer::new(dir);
        for (title, status) in [
            ("open task", "open"),
            ("closed task", "closed"),
            ("done task", "done"),
        ] {
            server
                .handle_create(TodoCreateParams {
                    title: title.to_string(),
                    tags: vec![],
                    plan: "plan-a".to_string(),
                    status: Some(status.to_string()),
                    body: None,
                    key: None,
                    dependencies: vec![],
                })
                .unwrap();
        }

        let closed = server
            .handle_list(TodoListParams {
                filter: "closed".to_string(),
                tag: None,
                plan: Some("plan-a".to_string()),
            })
            .unwrap();
        let closed_todos: Vec<Value> = serde_json::from_str(&extract_text(closed)).unwrap();
        assert_eq!(closed_todos.len(), 2);
        assert!(closed_todos
            .iter()
            .all(|todo| is_closed(todo["status"].as_str().unwrap())));

        let done = server
            .handle_list(TodoListParams {
                filter: "done".to_string(),
                tag: None,
                plan: Some("plan-a".to_string()),
            })
            .unwrap();
        let done_todos: Vec<Value> = serde_json::from_str(&extract_text(done)).unwrap();
        assert_eq!(done_todos.len(), 2);
        assert!(done_todos
            .iter()
            .all(|todo| is_closed(todo["status"].as_str().unwrap())));

        let open = server
            .handle_list(TodoListParams {
                filter: "open".to_string(),
                tag: None,
                plan: Some("plan-a".to_string()),
            })
            .unwrap();
        let open_todos: Vec<Value> = serde_json::from_str(&extract_text(open)).unwrap();
        assert_eq!(open_todos.len(), 1);
        assert_eq!(open_todos[0]["status"], "open");
    }

    #[test]
    fn plan_add_note_multiple_notes_create_separate_files() {
        let dir = temp_test_dir("plan-add-note-multi");
        let server = TodoServer::new(dir.clone());

        let r1 = server
            .handle_plan_add_note(PlanAddNoteParams {
                name: "plan-a".to_string(),
                text: "note one".to_string(),
            })
            .unwrap();
        let r2 = server
            .handle_plan_add_note(PlanAddNoteParams {
                name: "plan-a".to_string(),
                text: "note two".to_string(),
            })
            .unwrap();

        let s1 = extract_text(r1);
        let s2 = extract_text(r2);
        assert!(s1.contains("Created note"));
        assert!(s2.contains("Created note"));
        assert_ne!(s1, s2);

        let plan_dir = dir.join("plan-a");
        let note_files: Vec<_> = fs::read_dir(&plan_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("note-"))
            .collect();
        assert_eq!(note_files.len(), 2);
        assert!(!plan_dir.join("plan.md").exists());
    }

    #[test]
    fn read_plan_returns_json_with_note_and_todo_ids() {
        let dir = temp_test_dir("read-plan-json");
        let server = TodoServer::new(dir.clone());

        server
            .handle_plan_write(PlanWriteParams {
                name: "my-plan".to_string(),
                content: "# My Plan\n".to_string(),
                todos: None,
            })
            .unwrap();

        let note_result = server
            .handle_plan_add_note(PlanAddNoteParams {
                name: "my-plan".to_string(),
                text: "some note".to_string(),
            })
            .unwrap();
        let note_summary = extract_text(note_result);
        let note_id = note_summary
            .split_whitespace()
            .nth(2)
            .unwrap()
            .to_string();

        let todo_result = server
            .handle_create(TodoCreateParams {
                title: "task one".to_string(),
                tags: vec![],
                plan: "my-plan".to_string(),
                status: None,
                body: None,
                key: None,
                dependencies: vec![],
            })
            .unwrap();
        let todo_json: Value = serde_json::from_str(&extract_text(todo_result)).unwrap();
        let todo_id = todo_json["id"].as_str().unwrap().to_string();

        let read_result = server
            .handle_plan_read(PlanReadParams {
                name: "my-plan".to_string(),
            })
            .unwrap();
        let overview: Value = serde_json::from_str(&extract_text(read_result)).unwrap();

        assert_eq!(overview["plan"], "my-plan");
        assert!(overview["content"].as_str().unwrap().contains("# My Plan"));

        let note_ids: Vec<String> = serde_json::from_value(overview["note_ids"].clone()).unwrap();
        let todo_ids: Vec<String> = serde_json::from_value(overview["todo_ids"].clone()).unwrap();

        assert_eq!(note_ids.len(), 1);
        assert!(note_ids.contains(&note_id));

        assert_eq!(todo_ids.len(), 1);
        assert!(todo_ids.contains(&todo_id));
    }

    #[test]
    fn plan_read_note_returns_content() {
        let dir = temp_test_dir("plan-read-note-content");
        let server = TodoServer::new(dir.clone());

        let add_result = server
            .handle_plan_add_note(PlanAddNoteParams {
                name: "plan-a".to_string(),
                text: "my important note".to_string(),
            })
            .unwrap();
        let summary = extract_text(add_result);
        let note_id = summary.split_whitespace().nth(2).unwrap().to_string();

        let read_result = server
            .handle_plan_read_note(PlanReadNoteParams {
                plan: "plan-a".to_string(),
                note_id: note_id.clone(),
            })
            .unwrap();
        let content = extract_text(read_result);
        assert!(content.contains("my important note"));
    }

    #[test]
    fn plan_read_note_returns_error_for_missing_note() {
        let dir = temp_test_dir("plan-read-note-missing");
        let server = TodoServer::new(dir.clone());

        fs::create_dir_all(dir.join("plan-a")).unwrap();

        let result = server.handle_plan_read_note(PlanReadNoteParams {
            plan: "plan-a".to_string(),
            note_id: "deadbeef".to_string(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn plan_read_note_normalizes_note_id_variants() {
        let dir = temp_test_dir("plan-read-note-normalizes");
        let server = TodoServer::new(dir.clone());

        let add_result = server
            .handle_plan_add_note(PlanAddNoteParams {
                name: "plan-a".to_string(),
                text: "normalized note text".to_string(),
            })
            .unwrap();
        let summary = extract_text(add_result);
        let note_id = summary.split_whitespace().nth(2).unwrap().to_string();

        for variant in [
            format!("note-{note_id}"),
            note_id.to_uppercase(),
            format!(" {note_id} "),
        ] {
            let read_result = server
                .handle_plan_read_note(PlanReadNoteParams {
                    plan: "plan-a".to_string(),
                    note_id: variant,
                })
                .unwrap();
            let content = extract_text(read_result);
            assert!(content.contains("normalized note text"));
        }
    }

    #[test]
    fn read_plan_returns_json_for_note_only_plan() {
        let dir = temp_test_dir("read-plan-note-only");
        let server = TodoServer::new(dir.clone());

        server
            .handle_plan_add_note(PlanAddNoteParams {
                name: "note-only-plan".to_string(),
                text: "orphan note".to_string(),
            })
            .unwrap();

        let read_result = server
            .handle_plan_read(PlanReadParams {
                name: "note-only-plan".to_string(),
            })
            .unwrap();
        let overview: Value = serde_json::from_str(&extract_text(read_result)).unwrap();

        let note_ids: Vec<String> = serde_json::from_value(overview["note_ids"].clone()).unwrap();
        let todo_ids: Vec<String> = serde_json::from_value(overview["todo_ids"].clone()).unwrap();

        assert_eq!(note_ids.len(), 1);
        assert_eq!(todo_ids.len(), 0);
        assert_eq!(overview["content"], "");
    }
}

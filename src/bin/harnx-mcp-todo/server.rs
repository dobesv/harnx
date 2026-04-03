//! Todo MCP server implementation.
//!
//! Stores todos as JSON-frontmatter + markdown body files in a configurable directory.
//! File format: `<8-hex-id>.md` containing a JSON header block followed by markdown body.

use rmcp::model::{
    CallToolRequestParam, CallToolResult, Content, ErrorData, Implementation, ListToolsResult,
    PaginatedRequestParam, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::schemars::{generate::SchemaGenerator, JsonSchema, Schema};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::path::{Path, PathBuf};

// ── Data types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TodoFrontMatter {
    id: String,
    title: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "default_status")]
    status: String,
    #[serde(default)]
    created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
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

// ── Tool parameter structs ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TodoListParams {
    /// Filter: "open" (default), "closed", "all"
    #[serde(default = "default_filter")]
    filter: String,
    /// Optional tag filter
    #[serde(default)]
    tag: Option<String>,
}

fn default_filter() -> String {
    "open".to_string()
}

#[derive(Debug, Deserialize)]
struct TodoGetParams {
    /// Todo ID (hex or TODO-hex)
    id: String,
}

#[derive(Debug, Deserialize)]
struct TodoCreateParams {
    /// Short title
    title: String,
    /// Optional tags
    #[serde(default)]
    tags: Vec<String>,
    /// Initial status (default: "open")
    #[serde(default)]
    status: Option<String>,
    /// Optional markdown body
    #[serde(default)]
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TodoUpdateParams {
    /// Todo ID
    id: String,
    /// New title (optional)
    #[serde(default)]
    title: Option<String>,
    /// New status (optional)
    #[serde(default)]
    status: Option<String>,
    /// New tags (optional, replaces all)
    #[serde(default)]
    tags: Option<Vec<String>>,
    /// New body (optional, replaces body)
    #[serde(default)]
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TodoAppendParams {
    /// Todo ID
    id: String,
    /// Text to append to body
    text: String,
}

#[derive(Debug, Deserialize)]
struct TodoDeleteParams {
    /// Todo ID
    id: String,
}

// ── JsonSchema impls ────────────────────────────────────────────────────────

macro_rules! impl_json_schema {
    ($name:ident, $schema_name:literal, $props:expr, $required:expr) => {
        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed($schema_name)
            }
            fn json_schema(generator: &mut SchemaGenerator) -> Schema {
                let props: Vec<(&str, &str, Schema)> = $props(generator);
                let required: &[&str] = $required;
                object_schema_with_desc(props, required)
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
            "Filter: 'open' (default), 'closed', or 'all'",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "tag",
            "Optional tag to filter by",
            gen.subschema_for::<Option<String>>()
        ),
    ],
    &[]
);

impl_json_schema!(
    TodoGetParams,
    "TodoGetParams",
    |gen: &mut SchemaGenerator| vec![(
        "id",
        "Todo ID (8-char hex, or TODO-<hex>)",
        gen.subschema_for::<String>()
    ),],
    &["id"]
);

impl_json_schema!(
    TodoCreateParams,
    "TodoCreateParams",
    |gen: &mut SchemaGenerator| vec![
        (
            "title",
            "Short summary shown in lists",
            gen.subschema_for::<String>()
        ),
        (
            "tags",
            "Optional tags",
            gen.subschema_for::<Option<Vec<String>>>()
        ),
        (
            "status",
            "Initial status (default: 'open')",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "body",
            "Long-form markdown details",
            gen.subschema_for::<Option<String>>()
        ),
    ],
    &["title"]
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
            "status",
            "New status (optional)",
            gen.subschema_for::<Option<String>>()
        ),
        (
            "tags",
            "New tags (replaces all, optional)",
            gen.subschema_for::<Option<Vec<String>>>()
        ),
        (
            "body",
            "New body (replaces, optional)",
            gen.subschema_for::<Option<String>>()
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
            "Text to append to the body (markdown)",
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

// ── Helpers ─────────────────────────────────────────────────────────────────

const TODO_ID_PREFIX: &str = "TODO-";

fn normalize_id(raw: &str) -> String {
    let s = raw.trim().trim_start_matches('#');
    let s = if s.to_uppercase().starts_with(TODO_ID_PREFIX) {
        &s[TODO_ID_PREFIX.len()..]
    } else {
        s
    };
    s.to_lowercase()
}

fn display_id(id: &str) -> String {
    format!("{}{}", TODO_ID_PREFIX, id)
}

fn is_closed(status: &str) -> bool {
    matches!(status.to_lowercase().as_str(), "closed" | "done")
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

fn todo_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{}.md", id))
}

fn parse_todo_content(content: &str, id_fallback: &str) -> TodoRecord {
    if !content.starts_with('{') {
        return TodoRecord {
            front: TodoFrontMatter {
                id: id_fallback.to_string(),
                title: String::new(),
                tags: vec![],
                status: "open".to_string(),
                created_at: String::new(),
                updated_at: None,
            },
            body: content.to_string(),
        };
    }
    // Find the end of the JSON object
    let end = find_json_end(content);
    if end < 0 {
        return TodoRecord {
            front: TodoFrontMatter {
                id: id_fallback.to_string(),
                title: String::new(),
                tags: vec![],
                status: "open".to_string(),
                created_at: String::new(),
                updated_at: None,
            },
            body: content.to_string(),
        };
    }
    let json_str = &content[..=(end as usize)];
    let body = content[(end as usize) + 1..]
        .trim_start_matches('\r')
        .trim_start_matches('\n')
        .to_string();

    let front: TodoFrontMatter = serde_json::from_str(json_str).unwrap_or(TodoFrontMatter {
        id: id_fallback.to_string(),
        title: String::new(),
        tags: vec![],
        status: "open".to_string(),
        created_at: String::new(),
        updated_at: None,
    });

    TodoRecord { front, body }
}

fn find_json_end(content: &str) -> i64 {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, ch) in content.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
            continue;
        }
        if ch == '{' {
            depth += 1;
        }
        if ch == '}' {
            depth -= 1;
            if depth == 0 {
                return i as i64;
            }
        }
    }
    -1
}

fn serialize_todo(todo: &TodoRecord) -> String {
    let json = serde_json::to_string_pretty(&todo.front).unwrap_or_default();
    let body = todo.body.trim();
    if body.is_empty() {
        format!("{}\n", json)
    } else {
        format!("{}\n\n{}\n", json, body)
    }
}

fn read_todo(dir: &Path, id: &str) -> Result<TodoRecord, String> {
    let path = todo_path(dir, id);
    let content =
        std::fs::read_to_string(&path).map_err(|_| format!("{} not found", display_id(id)))?;
    Ok(parse_todo_content(&content, id))
}

fn write_todo(dir: &Path, todo: &TodoRecord) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("Failed to create todo dir: {e}"))?;
    let path = todo_path(dir, &todo.front.id);
    std::fs::write(&path, serialize_todo(todo))
        .map_err(|e| format!("Failed to write todo: {e}"))?;
    Ok(())
}

fn list_todos(dir: &Path) -> Vec<TodoRecord> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };
    let mut todos = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".md") {
            continue;
        }
        let id = &name[..name.len() - 3];
        if let Ok(content) = std::fs::read_to_string(entry.path()) {
            todos.push(parse_todo_content(&content, id));
        }
    }
    // Sort: open first, then by created_at
    todos.sort_by(|a, b| {
        let a_closed = is_closed(&a.front.status);
        let b_closed = is_closed(&b.front.status);
        if a_closed != b_closed {
            return if a_closed {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            };
        }
        a.front.created_at.cmp(&b.front.created_at)
    });
    todos
}

fn todo_to_json(todo: &TodoRecord) -> Value {
    serde_json::json!({
        "id": display_id(&todo.front.id),
        "title": todo.front.title,
        "tags": todo.front.tags,
        "status": todo.front.status,
        "created_at": todo.front.created_at,
        "updated_at": todo.front.updated_at,
        "body": todo.body,
    })
}

fn todo_list_to_json(todos: &[TodoRecord]) -> Value {
    let open: Vec<Value> = todos
        .iter()
        .filter(|t| !is_closed(&t.front.status))
        .map(todo_to_json)
        .collect();
    let closed: Vec<Value> = todos
        .iter()
        .filter(|t| is_closed(&t.front.status))
        .map(todo_to_json)
        .collect();
    serde_json::json!({ "open": open, "closed": closed })
}

// ── Server ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TodoServer {
    dir: PathBuf,
}

impl TodoServer {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn handle_list(&self, params: TodoListParams) -> Result<CallToolResult, ErrorData> {
        let all = list_todos(&self.dir);
        let filtered: Vec<TodoRecord> = all
            .into_iter()
            .filter(|t| match params.filter.as_str() {
                "closed" | "done" => is_closed(&t.front.status),
                "all" => true,
                _ => !is_closed(&t.front.status), // "open" default
            })
            .filter(|t| {
                if let Some(ref tag) = params.tag {
                    t.front.tags.iter().any(|tg| tg.eq_ignore_ascii_case(tag))
                } else {
                    true
                }
            })
            .collect();
        let json = if params.filter == "all" {
            todo_list_to_json(&filtered)
        } else {
            serde_json::json!(filtered.iter().map(todo_to_json).collect::<Vec<_>>())
        };
        let text = serde_json::to_string_pretty(&json).unwrap_or_default();
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    fn handle_get(&self, params: TodoGetParams) -> Result<CallToolResult, ErrorData> {
        let id = normalize_id(&params.id);
        match read_todo(&self.dir, &id) {
            Ok(todo) => {
                let text = serde_json::to_string_pretty(&todo_to_json(&todo)).unwrap_or_default();
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }

    fn handle_create(&self, params: TodoCreateParams) -> Result<CallToolResult, ErrorData> {
        let id = generate_id();
        // Retry if collision (unlikely)
        if todo_path(&self.dir, &id).exists() {
            return Ok(CallToolResult::error(vec![Content::text(
                "ID collision, please retry",
            )]));
        }
        let todo = TodoRecord {
            front: TodoFrontMatter {
                id: id.clone(),
                title: params.title,
                tags: params.tags,
                status: params.status.unwrap_or_else(|| "open".to_string()),
                created_at: now_iso(),
                updated_at: None,
            },
            body: params.body.unwrap_or_default(),
        };
        if let Err(e) = write_todo(&self.dir, &todo) {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }
        let text = serde_json::to_string_pretty(&todo_to_json(&todo)).unwrap_or_default();
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    fn handle_update(&self, params: TodoUpdateParams) -> Result<CallToolResult, ErrorData> {
        let id = normalize_id(&params.id);
        let mut todo = match read_todo(&self.dir, &id) {
            Ok(t) => t,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(e)])),
        };
        if let Some(title) = params.title {
            todo.front.title = title;
        }
        if let Some(status) = params.status {
            todo.front.status = status;
        }
        if let Some(tags) = params.tags {
            todo.front.tags = tags;
        }
        if let Some(body) = params.body {
            todo.body = body;
        }
        todo.front.updated_at = Some(now_iso());
        if let Err(e) = write_todo(&self.dir, &todo) {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }
        let text = serde_json::to_string_pretty(&todo_to_json(&todo)).unwrap_or_default();
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    fn handle_append(&self, params: TodoAppendParams) -> Result<CallToolResult, ErrorData> {
        let id = normalize_id(&params.id);
        let mut todo = match read_todo(&self.dir, &id) {
            Ok(t) => t,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(e)])),
        };
        let spacer = if todo.body.trim().is_empty() {
            ""
        } else {
            "\n\n"
        };
        todo.body = format!("{}{}{}", todo.body.trim_end(), spacer, params.text.trim());
        todo.front.updated_at = Some(now_iso());
        if let Err(e) = write_todo(&self.dir, &todo) {
            return Ok(CallToolResult::error(vec![Content::text(e)]));
        }
        let text = serde_json::to_string_pretty(&todo_to_json(&todo)).unwrap_or_default();
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    fn handle_delete(&self, params: TodoDeleteParams) -> Result<CallToolResult, ErrorData> {
        let id = normalize_id(&params.id);
        let path = todo_path(&self.dir, &id);
        if !path.exists() {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "{} not found",
                display_id(&id)
            ))]));
        }
        std::fs::remove_file(&path)
            .map_err(|e| ErrorData::internal_error(format!("delete failed: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "{} deleted",
            display_id(&id)
        ))]))
    }
}

impl ServerHandler for TodoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: Default::default(),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "harnx-mcp-todo".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: None,
                website_url: None,
                icons: None,
            },
            instructions: Some(
                "File-based todo/plan management. Todos stored as markdown files with JSON front matter."
                    .to_string(),
            ),
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: vec![
                Tool::new(
                    "todo_list",
                    "List todos. Filter by status ('open', 'closed', 'all') and optionally by tag.",
                    Map::new(),
                )
                .with_input_schema::<TodoListParams>(),
                Tool::new(
                    "todo_get",
                    "Get a single todo by ID, including its full body.",
                    Map::new(),
                )
                .with_input_schema::<TodoGetParams>(),
                Tool::new(
                    "todo_create",
                    "Create a new todo with title, optional tags, status, and body.",
                    Map::new(),
                )
                .with_input_schema::<TodoCreateParams>(),
                Tool::new(
                    "todo_update",
                    "Update a todo's title, status, tags, or body (replaces).",
                    Map::new(),
                )
                .with_input_schema::<TodoUpdateParams>(),
                Tool::new(
                    "todo_append",
                    "Append text to a todo's body (adds, doesn't replace).",
                    Map::new(),
                )
                .with_input_schema::<TodoAppendParams>(),
                Tool::new("todo_delete", "Delete a todo by ID.", Map::new())
                    .with_input_schema::<TodoDeleteParams>(),
            ],
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParam,
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

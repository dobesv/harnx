use std::borrow::Cow;
use std::sync::Arc;

use jiff::Timestamp;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorData, Implementation, ListToolsResult,
    Meta, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::schemars::{generate::SchemaGenerator, JsonSchema, Schema};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use similar::{ChangeTag, TextDiff};

use crate::model::{
    NewNote, NewPlan, NewTask, Note, NoteMetaUpdate, Plan, PlanId, PlanMetaUpdate, Task,
    TaskFilter, TaskMetaUpdate,
};
use crate::store::{PlanStore, StoreError};

mod handler;
mod handlers;
mod params;

pub use handler::{serve_plans_server, serve_plans_server_with_meta};
pub use params::*;

#[derive(Debug, Clone)]
pub struct ServerMeta {
    pub name: &'static str,
    pub instructions: &'static str,
}

impl ServerMeta {
    pub const fn new(name: &'static str, instructions: &'static str) -> Self {
        Self { name, instructions }
    }
}

pub const DEFAULT_SERVER_META: ServerMeta = ServerMeta::new(
    "harnx-mcp-plans",
    "File-based plan/task/note management server using markdown + YAML front matter",
);

pub struct PlansServer<S: PlanStore> {
    pub(crate) store: Arc<S>,
    pub(crate) meta: ServerMeta,
}

impl<S: PlanStore> PlansServer<S> {
    pub fn new(store: Arc<S>) -> Self {
        Self::with_meta(store, DEFAULT_SERVER_META)
    }

    pub fn with_meta(store: Arc<S>, meta: ServerMeta) -> Self {
        Self { store, meta }
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

fn timestamp_to_string(timestamp: Timestamp) -> String {
    timestamp.to_string()
}

fn optional_timestamp_to_string(timestamp: Option<Timestamp>) -> Option<String> {
    timestamp.map(timestamp_to_string)
}

fn parse_arguments<T: serde::de::DeserializeOwned>(
    args: Option<Map<String, Value>>,
) -> Result<T, ErrorData> {
    serde_json::from_value(Value::Object(args.unwrap_or_default()))
        .map_err(|err| ErrorData::invalid_params(err.to_string(), None))
}

fn result_json(value: Value) -> Result<CallToolResult, ErrorData> {
    let text = serde_json::to_string_pretty(&value)
        .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
    result_text(text)
}

fn result_text(text: String) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

#[allow(dead_code)]
fn result_with_diff(summary: String, diff: String) -> Result<CallToolResult, ErrorData> {
    if diff.is_empty() {
        return result_text(summary);
    }
    result_text(format!("{summary}\n\n```diff\n{diff}\n```"))
}

fn diff_text(old: &str, new: &str, path: &str) -> String {
    if old == new {
        return String::new();
    }
    let diff = TextDiff::from_lines(old, new);
    let mut rendered = format!("--- a/{path}\n+++ b/{path}\n");
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => '-',
            ChangeTag::Insert => '+',
            ChangeTag::Equal => ' ',
        };
        rendered.push(sign);
        rendered.push_str(change.value());
        if !change.value().ends_with('\n') {
            rendered.push('\n');
        }
    }
    rendered
}

fn apply_replace_in(body: &str, r: &ReplaceInContent) -> Result<String, ErrorData> {
    if r.old_text.is_empty() {
        return Err(ErrorData::invalid_params(
            "replace_in_body.old_text must not be empty",
            None,
        ));
    }
    if !body.contains(&r.old_text) {
        return Err(ErrorData::invalid_params(
            "replace_in_body.old_text not found in body",
            None,
        ));
    }
    let result = if r.replace_all == Some(true) {
        body.replace(&r.old_text, &r.new_text)
    } else {
        body.replacen(&r.old_text, &r.new_text, 1)
    };
    Ok(result)
}

fn store_error_to_error_data(err: StoreError) -> ErrorData {
    match err {
        StoreError::NotFound => ErrorData::invalid_params("not found", None),
        StoreError::AlreadyExists => ErrorData::invalid_params("already exists", None),
        StoreError::InvalidId(message) | StoreError::InvalidParams(message) => {
            ErrorData::invalid_params(message, None)
        }
        StoreError::RateLimited { retry_after_secs } => ErrorData::new(
            rmcp::model::ErrorCode(-32001),
            format!("rate limited; retry after {retry_after_secs}s"),
            Some(json!({ "retry_after_secs": retry_after_secs })),
        ),
        StoreError::Backend(err) => ErrorData::internal_error(err.to_string(), None),
    }
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

            fn json_schema(generator: &mut SchemaGenerator) -> Schema {
                object_schema_with_desc($properties_fn(generator), $required)
            }
        }
    };
}

pub(crate) use impl_json_schema;

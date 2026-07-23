use std::borrow::Cow;
use std::sync::Arc;

use jiff::Timestamp;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ErrorData, Implementation,
    ListToolsResult, Meta, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::schemars::{generate::SchemaGenerator, JsonSchema, Schema};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use similar::{ChangeTag, TextDiff};

use crate::model::{
    NewNote, NewPlan, NewTask, Note, NoteMetaUpdate, Plan, PlanId, PlanMetaUpdate, RepoTarget,
    Target, Task, TaskFilter, TaskMetaUpdate,
};
use crate::store::{PlanStore, StoreError};

mod handler;
mod handlers;
mod params;

pub use handler::{serve_plans_server, serve_plans_server_with_meta};
pub use params::*;

/// Repository targeting policy exposed by a plans server.
#[derive(Debug, Clone, Default)]
pub enum TargetPolicy {
    /// Backend has no remote repository target; owner/repo params are ignored.
    #[default]
    None,
    /// GitHub backend with an optional startup-detected default repository.
    GitHub { default_repo: Option<RepoTarget> },
}

#[derive(Debug, Clone)]
pub struct ServerMeta {
    pub name: Cow<'static, str>,
    pub instructions: Cow<'static, str>,
    pub target_policy: TargetPolicy,
}

impl ServerMeta {
    pub const fn new(name: &'static str, instructions: &'static str) -> Self {
        Self {
            name: Cow::Borrowed(name),
            instructions: Cow::Borrowed(instructions),
            target_policy: TargetPolicy::None,
        }
    }

    pub fn with_target_policy(mut self, target_policy: TargetPolicy) -> Self {
        self.target_policy = target_policy;
        self
    }
}

impl TargetPolicy {
    fn apply_to_tool_schema(&self, tool: &mut Tool) {
        let Self::GitHub { default_repo } = self else {
            return;
        };

        let mut schema = tool.input_schema.as_ref().clone();
        let properties = schema
            .entry("properties".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        let Value::Object(properties) = properties else {
            return;
        };

        let description = match default_repo {
            Some(default_repo) => format!(
                "Optional. Defaults to {}/{} detected at startup.",
                default_repo.owner, default_repo.repo
            ),
            None => "Required. No default repository was detected at startup.".to_string(),
        };
        let property_schema = |description: &str| {
            let mut property = Map::new();
            property.insert("type".to_string(), Value::String("string".to_string()));
            property.insert(
                "description".to_string(),
                Value::String(description.to_string()),
            );
            Value::Object(property)
        };
        properties.insert("owner".to_string(), property_schema(&description));
        properties.insert("repo".to_string(), property_schema(&description));

        if default_repo.is_none() {
            let required = schema
                .entry("required".to_string())
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Value::Array(required) = required {
                for name in ["owner", "repo"] {
                    if !required.iter().any(|value| value.as_str() == Some(name)) {
                        required.push(Value::String(name.to_string()));
                    }
                }
            }
        }

        tool.input_schema = Arc::new(schema);
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

    /// Resolve and validate one per-call storage target from tool arguments and server defaults.
    pub(crate) fn resolve_target(
        &self,
        owner: Option<&str>,
        repo: Option<&str>,
    ) -> Result<Target, ErrorData> {
        match &self.meta.target_policy {
            TargetPolicy::None => Ok(Target::Local),
            TargetPolicy::GitHub { default_repo } => {
                let owner = owner.and_then(non_empty_trimmed);
                let repo = repo.and_then(non_empty_trimmed);
                match (owner, repo) {
                    (Some(owner), Some(repo)) => RepoTarget::new(owner, repo)
                        .map(Target::GitHub)
                        .map_err(|message| ErrorData::invalid_params(message, None)),
                    (Some(_), None) | (None, Some(_)) => Err(ErrorData::invalid_params(
                        "owner and repo must be provided together",
                        None,
                    )),
                    (None, None) => {
                        let default_repo = default_repo.clone().ok_or_else(|| {
                            ErrorData::invalid_params(
                                "owner and repo are required because no default GitHub repository was detected at startup",
                                None,
                            )
                        })?;
                        default_repo
                            .validate()
                            .map_err(|message| ErrorData::invalid_params(message, None))?;
                        Ok(Target::GitHub(default_repo))
                    }
                }
            }
        }
    }
}

fn non_empty_trimmed(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
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
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
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

use crate::summary::{
    apply_search_notices, find_summary, ls_summary, render_read_result, search_summary,
    SearchTruncation,
};

use harnx_mcp::peer::peer_supports_roots;
use harnx_mcp::safety::{
    file_uri_to_path, format_size, is_binary_content, sanitize_output_text, truncate_line,
    validate_path, validate_write_path, DEFAULT_FIND_LIMIT, DEFAULT_GREP_LIMIT, DEFAULT_LS_LIMIT,
    DEFAULT_MAX_LINES, GREP_MAX_LINE_LENGTH, LS_SCAN_HARD_LIMIT, READ_MAX_FILE_BYTES,
    SEARCH_FILE_MAX_BYTES, WRITE_MAX_BYTES,
};
use harnx_mcp::schema::object_schema_with_desc;
use harnx_mcp_history::HistoryManager;

use fancy_regex::Regex;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ErrorData, Implementation,
    ListToolsResult, Meta, PaginatedRequestParams, Role, ServerCapabilities, ServerInfo, Tool,
    ToolAnnotations,
};
use rmcp::schemars::{generate::SchemaGenerator, JsonSchema, Schema};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{Mutex as AsyncMutex, RwLock};

mod handler;
mod handlers;
mod params;
#[cfg(test)]
pub(crate) mod tests;
mod walk;

pub use params::*;
pub(crate) use walk::*;

#[derive(Clone)]
pub struct FsServer {
    roots: Arc<RwLock<Vec<PathBuf>>>,
    configured_roots: Arc<Vec<PathBuf>>,
    roots_initialized: Arc<AtomicBool>,
    default_root_cwd: bool,
    history: Arc<HistoryManager>,
    repo_locks: Arc<Mutex<HashMap<PathBuf, Weak<RwLock<()>>>>>,
    file_locks: Arc<Mutex<HashMap<PathBuf, Weak<AsyncMutex<()>>>>>,
}

/// Build a `_meta` block with only the call template. We deliberately
/// omit `result_template` so the MCP client falls back to its generic
/// audience-aware renderer (`extract_user_display_text`), which surfaces
/// every user-audience and unaudienced content block. That includes the
/// history diff that mutating tools append after the summary.
fn make_tool_meta(call_template: &str) -> Meta {
    Meta(
        json!({ "call_template": call_template })
            .as_object()
            .unwrap()
            .clone(),
    )
}

fn parse_arguments<T: DeserializeOwned>(
    arguments: Option<Map<String, Value>>,
) -> Result<T, ErrorData> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|err| ErrorData::invalid_params(format!("invalid tool arguments: {err}"), None))
}

fn tool_error(msg: impl Into<String>) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::error(vec![ContentBlock::text(msg.into())]))
}

fn invalid_params(msg: impl Into<Cow<'static, str>>) -> ErrorData {
    ErrorData::invalid_params(msg, None)
}

fn internal_error(msg: impl Into<Cow<'static, str>>) -> ErrorData {
    ErrorData::internal_error(msg, None)
}

fn default_search_path(roots: &[PathBuf]) -> PathBuf {
    roots
        .first()
        .cloned()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

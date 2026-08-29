use harnx_core::execution_context::{
    ExecutionContextObservation, ToolObservationProvenance, EXECUTION_CONTEXT_NAMESPACE,
};
use harnx_core::safety::{format_size, sanitize_output_text, truncate_output, TruncateOpts};

use fancy_regex::Regex;
use gix::ObjectId;
use harnx_mcp_history::classify::{classify_command, SnapshotDecision};
use harnx_mcp_history::HistoryManager;
#[cfg(unix)]
use harnx_sandbox_common::build_default_sandbox_args;
use harnx_sandbox_common::SandboxConfig;
use harnx_tool_allow::{validate_path, validate_write_path, ResolvedAllowlist};
use harnx_toolset_server::schema::object_schema_with_desc;
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    Implementation, ListToolsResult, MetaObject, PaginatedRequestParams, Role, ServerCapabilities,
    ServerInfo, Tool,
};
use rmcp::schemars::{generate::SchemaGenerator, JsonSchema, Schema};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
#[cfg(unix)]
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File as TokioFile;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use uuid::Uuid;

mod command;
mod env;
mod exec;
mod exec_log;
mod handler;
mod handlers;
mod lifecycle;
mod params;
mod process;
mod render;
#[cfg(all(test, not(target_os = "windows")))]
mod tests;

pub(crate) use handlers::*;
pub(crate) use params::*;
pub(crate) use render::*;

use crate::tool_template::ToolTemplate;
use crate::tool_templates;

pub(crate) const BUILTIN_TOOL_NAMES: &[&str] = &[
    "exec",
    "read_exec_log",
    "spawn",
    "wait",
    "terminate",
    "rollback_file",
];

#[derive(Clone)]
pub(crate) struct RegisteredToolTemplate {
    pub(crate) template: ToolTemplate,
    pub(crate) description: String,
    pub(crate) input_schema: Map<String, Value>,
    pub(crate) read_paths: Vec<PathBuf>,
    pub(crate) write_paths: Vec<PathBuf>,
    pub(crate) pass_env: Vec<String>,
    pub(crate) sandbox_enabled: bool,
    pub(crate) no_network: bool,
    pub(crate) ignored_grants: bool,
}

#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) struct TemplateSandbox<'a> {
    pub(crate) enabled: bool,
    pub(crate) read_paths: &'a [PathBuf],
    pub(crate) write_paths: &'a [PathBuf],
    pub(crate) pass_env: &'a [String],
    pub(crate) no_network: bool,
}

/// Build a `_meta` block carrying only the call template. Bash tools omit
/// `result_template` so the client keeps its audience-aware renderer, which is
/// what surfaces the history diff blocks mutating tools append to their output.
fn call_template_meta(call_template: &str) -> MetaObject {
    MetaObject(
        json!({ "call_template": call_template })
            .as_object()
            .expect("object literal")
            .clone(),
    )
}

fn request_wants_execution_context(request: &CallToolRequestParams) -> bool {
    request
        .meta
        .as_ref()
        .and_then(|meta| meta.0 .0.get(EXECUTION_CONTEXT_NAMESPACE))
        .is_some()
}

fn finalize_direct_mcp_context(
    mut result: CallToolResult,
    enabled: bool,
    tool_name: &str,
    call_id: String,
) -> CallToolResult {
    let raw = result
        .meta
        .as_mut()
        .and_then(|meta| meta.0.remove(EXECUTION_CONTEXT_NAMESPACE));
    if result.meta.as_ref().is_some_and(|meta| meta.0.is_empty()) {
        result.meta = None;
    }
    if !enabled {
        return result;
    }
    let Some(raw) = raw else {
        return result;
    };
    let Ok(mut observation) = serde_json::from_value::<ExecutionContextObservation>(raw) else {
        log::warn!("stripping malformed bash execution-context metadata");
        return result;
    };
    observation.provenance = Some(ToolObservationProvenance::new(
        "mcp",
        "harnx-bash-tools",
        tool_name,
        call_id,
    ));
    result.meta.get_or_insert_with(MetaObject::new).0.insert(
        EXECUTION_CONTEXT_NAMESPACE.to_string(),
        serde_json::to_value(observation).expect("execution context serializes"),
    );
    result
}

// Spawned process tracking
pub(crate) struct SpawnedProcess {
    child: Box<dyn ChildWrapper>,
    command: String,
    working_dir: PathBuf,
    stdout_log_path: PathBuf,
    stderr_log_path: PathBuf,
    before_snap_ids: Vec<(PathBuf, gix::ObjectId)>,
    snapshot_decision: SnapshotDecision,
}

struct BashServerInner {
    allowlist: Arc<ResolvedAllowlist>,
    spawned: Mutex<HashMap<String, SpawnedProcess>>,
    log_dir: PathBuf,
    history: Arc<HistoryManager>,
    /// Sandbox + env config. Sandbox-specific fields (`enabled`, `allowlist`,
    /// `sandbox_run_path`) are only used on Unix; env fields
    /// (`extra_env_passthrough`, `env_overrides`) are
    /// honoured on every platform.
    sandbox_config: SandboxConfig,
    templates: BTreeMap<String, RegisteredToolTemplate>,
}

#[derive(Clone)]
pub struct BashServer {
    inner: Arc<BashServerInner>,
}

use harnx_mcp::safety::{
    default_root_from_cwd, file_uri_to_path, format_size, sanitize_output_text, truncate_output,
    validate_path, TruncateOpts,
};

use fancy_regex::Regex;
use gix::ObjectId;
use harnx_mcp::peer::peer_supports_roots;
use harnx_mcp::schema::object_schema_with_desc;
use harnx_mcp_history::classify::{classify_command, SnapshotDecision};
use harnx_mcp_history::HistoryManager;
#[cfg(unix)]
use harnx_sandbox_common::build_default_sandbox_args;
use harnx_sandbox_common::SandboxConfig;
#[cfg(unix)]
use harnx_sandbox_common::SYSTEM_EXEC_PATHS;
#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ErrorData, Implementation,
    ListToolsResult, Meta, PaginatedRequestParams, Role, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::schemars::{generate::SchemaGenerator, JsonSchema, Schema};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::borrow::Cow;
use std::collections::HashMap;
#[cfg(unix)]
use std::ffi::OsString;
use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::fs::File as TokioFile;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, RwLock};
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
#[cfg(unix)]
mod sandbox;
#[cfg(all(test, not(target_os = "windows")))]
mod tests;

pub(crate) use handlers::*;
pub(crate) use params::*;
pub(crate) use render::*;
#[cfg(unix)]
pub(crate) use sandbox::*;

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
    roots: RwLock<Vec<PathBuf>>,
    initial_roots: Vec<PathBuf>,
    roots_initialized: AtomicBool,
    default_root_cwd: bool,
    spawned: Mutex<HashMap<String, SpawnedProcess>>,
    log_dir: PathBuf,
    history: Arc<HistoryManager>,
    /// Sandbox + env config. Sandbox-specific fields (`enabled`,
    /// `extra_exec`, `extra_readable`, `sandbox_run_path`) are only used on
    /// Unix; env fields (`extra_env_passthrough`, `env_overrides`) are
    /// honoured on every platform.
    sandbox_config: SandboxConfig,
}

#[derive(Clone)]
pub struct BashServer {
    inner: Arc<BashServerInner>,
}
